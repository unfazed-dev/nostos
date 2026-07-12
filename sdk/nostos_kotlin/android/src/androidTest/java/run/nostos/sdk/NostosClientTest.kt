package run.nostos.sdk

import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith

import uniffi.nostos_kotlin.NostosClient

import java.io.OutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.nio.file.Files
import java.nio.file.Paths

/**
 * Tier-2 emulator-E2E proof: the SAME `SyncClient<SqliteStorage>` the sibling
 * SDKs drive constructs + serves an offline query through the UniFFI
 * `NostosClient` shape, on a real Android emulator. Mirrors
 * `sdk/nostos_swift/src/lib.rs::tests::nostos_client_offline_connect_query_round_trip`
 * and `nostos_tauri`'s offline smoke path.
 *
 * Success criterion: `query("SELECT 1 AS one")` returns a JSON row containing
 * `"one":1` (the SQLite round-trip landed through the bundled-sqlite + JNA +
 * UniFFI FFI stack), AND `checkpoint()` reports 0 for a fresh store.
 *
 * If this test fails to LOAD the library, the failure mode is an
 * `UnsatisfiedLinkError` during `NostosClient.Companion` static init — that
 * means `libnostos_kotlin.so` is missing from the test apk's `jniLibs/arm64-v8a/`.
 * If the test loads but the assert fails, the SQLite-or-tokio runtime is
 * misconfigured for Android.
 */
@RunWith(AndroidJUnit4::class)
class NostosClientTest {
    companion object {
        @JvmStatic
        @BeforeClass
        fun loadNatives() {
            // Force-load libnostos_kotlin.so explicitly before the JNA path runs,
            // so the diagnostic log proves our own .so loads on the device even
            // if JNA's libjnidispatch.so fails.
            try {
                System.loadLibrary("nostos_kotlin")
                Log.e("NostosClientTest", "SUCCESS: loaded libnostos_kotlin.so via System.loadLibrary")
            } catch (e: UnsatisfiedLinkError) {
                Log.e("NostosClientTest", "FAILED to load libnostos_kotlin.so", e)
            }
        }
    }

    @Test
    fun offline_connect_query_roundTrip() {
        // Construct the UniFFI NostosClient handle. The default constructor
        // loads `libnostos_kotlin.so` via JNA on first use of the namespace.
        val client = NostosClient(
            url = "ws://localhost:0",
            token = null,
            dbPath = ":memory:",
        )

        // Open the in-memory SQLite store + build the SyncClient. No network.
        client.connect()

        // Run the read-side query.
        val rowsJson = client.query("SELECT 1 AS one")
        assertTrue(
            "expected a one=1 row in the JSON, got: $rowsJson",
            rowsJson.contains("\"one\":1") || rowsJson.contains("\"one\": 1"),
        )

        // Fresh store reports Lsn(0) — confirms the checkpoint read path also
        // round-trips through the FFI boundary.
        val lsn = client.checkpoint()
        assertEquals("fresh store should report Lsn(0)", 0UL, lsn)
    }

    /**
     * Tier-2 LIVE replication E2E — the Kotlin slice of
     * `docs/plans/sdk-live-e2e-consolidation.md`. Drives the SAME two-direction
     * round-trip the Rust reference template
     * (`crates/nostos-client/tests/e2e_live_replication.rs`) and the Swift slice
     * (`sdk/nostos_swift/ios-test/Sources/NostosSmoke/main.swift`) prove, adapted
     * to Kotlin + UniFFI on Android:
     *
     *   1. connect() → subscribe("tasks") → run loop applies rows to cairn_data.
     *   2. POST /push to the host-side spine → server pushes a `tasks` row →
     *      poll query() until the row lands on-device → [kt-e2e] PUSH_OK.
     *   3. SDK write()s `kt-echo` to its durable outbox → the spine's echo
     *      WriteBack re-emits it → run loop applies it → poll query() →
     *      [kt-e2e] ECHO_OK.
     *
     * The spine (`target/debug/examples/e2e_server`) is spawned host-side by
     * the orchestrator (`scripts/run-live-e2e.sh`); its `NOSTOS_E2E_PORT` is
     * conveyed to this test via the `nostosPort` instrumentation argument
     * (`-PnostosPort=<port>` on `connectedDebugAndroidTest`). The Android
     * emulator reaches the host at `10.0.2.2` (the documented host-loopback
     * alias — `localhost` from inside the emulator is the emu's own loopback).
     *
     * If `nostosPort` is unset ("0") the test self-skips via `assumeTrue` so the
     * offline test still runs green without a spine (the prior Tier-2 default).
     */
    @Test
    fun live_connect_push_echo_roundTrip() {
        // ---- spine endpoint discovery ----
        val portArg = InstrumentationRegistry.getArguments().getString("nostosPort", "0")
        val port = portArg.toIntOrNull() ?: 0
        assumeTrue(
            "[kt-e2e] SKIP: nostosPort instrumentation arg unset (run via scripts/run-live-e2e.sh)",
            port > 0,
        )

        val host = "10.0.2.2" // documented Android-emulator → host-loopback alias
        val wsUrl = "ws://$host:$port/sync"
        Log.i("NostosClientTest", "[kt-e2e] BEGIN live round-trip; spine wsUrl=$wsUrl")

        // PID-unique DB so the engine's apply path + durable outbox survive
        // across the run loop's flush + echo re-emit (mirrors the Rust template
        // + Swift main.swift). Clean remove first so a stale file from a prior
        // run can't yield a false positive.
        val ctx = InstrumentationRegistry.getTargetContext()
        val dbPath = Paths.get(ctx.cacheDir.absolutePath, "nostos-kt-e2e.sqlite").toString()
        try { Files.deleteIfExists(Paths.get(dbPath)) } catch (_: Exception) { }

        val client = NostosClient(url = wsUrl, token = null, dbPath = dbPath)
        client.connect()
        Log.i("NostosClientTest", "[kt-e2e] connect() ok")
        client.subscribe("tasks")
        Log.i("NostosClientTest", "[kt-e2e] subscribe(\"tasks\") ok — run loop driving replication")

        // Let the subscribe land + the session register with the fan-out (the
        // spine only delivers to sessions registered at fan-out time). Mirrors
        // the Rust template's 500ms settle.
        Thread.sleep(500)

        // ---- direction 1: server PUSH → on-device query ----
        // The subscribe run-loop is fire-and-forget; on a cold Android start the
        // WS may not have registered with the spine's fan-out before the first
        // /push, so re-push (idempotent upsert by pk) in a loop until the row
        // lands. The spine delivers ONLY to sessions registered at fan-out
        // time — a push that races the subscribe is missed, not queued, so a
        // single shot flakes under cold-start latency.
        val pushBody = """{"pk":"kt-push","payload":{"title":"from-server","status":"open","priority":"5"}}"""
        val pushSql = "SELECT pk FROM cairn_data WHERE table_name='tasks' AND pk='kt-push'"
        val pushDeadline = System.currentTimeMillis() + 15_000L
        var pushOk = false
        var pushRows = ""
        while (System.currentTimeMillis() < pushDeadline && !pushOk) {
            httpPush(host, port, pushBody)
            val innerDeadline = System.currentTimeMillis() + 2_000L
            while (System.currentTimeMillis() < innerDeadline) {
                val rows = runCatching { client.query(pushSql) }.getOrDefault("")
                if (rows.contains("kt-push")) {
                    pushOk = true
                    break
                }
                Thread.sleep(100)
            }
            pushRows = runCatching { client.query(pushSql) }.getOrDefault("<query failed>")
        }
        assertTrue("[kt-e2e] kt-push never landed in cairn_data; rows=$pushRows", pushOk)
        Log.i("NostosClientTest", "[kt-e2e] PUSH_OK")

        // ---- direction 2: client WRITE → server echo → on-device query ----
        val echoPayload = """{"title":"from-kotlin","status":"open","priority":"7"}"""
        val writeId = client.write("tasks", "upsert", "kt-echo", echoPayload)
        Log.i("NostosClientTest", "[kt-e2e] write() id=$writeId (kt-echo enqueued)")
        val echoSql = "SELECT pk FROM cairn_data WHERE table_name='tasks' AND pk='kt-echo'"
        val echoOk = pollQueryContains(client, echoSql, "kt-echo", timeoutMillis = 8000L)
        val echoRows = runCatching { client.query(echoSql) }.getOrDefault("<query failed>")
        assertTrue("[kt-e2e] kt-echo never echoed back; rows=$echoRows", echoOk)
        Log.i("NostosClientTest", "[kt-e2e] ECHO_OK")
    }

    // ---- helpers ----

    /** Poll `client.query(sql)` until the result contains `needle` or the timeout fires. */
    private fun pollQueryContains(
        client: NostosClient,
        sql: String,
        needle: String,
        timeoutMillis: Long,
    ): Boolean {
        val deadline = System.currentTimeMillis() + timeoutMillis
        while (System.currentTimeMillis() < deadline) {
            try {
                if (client.query(sql).contains(needle)) return true
            } catch (e: Exception) {
                // query() can transiently fail if the run loop is mid-flush;
                // poll loop retries until the deadline.
            }
            Thread.sleep(100)
        }
        return false
    }

    /** POST a row to the spine's `/push` control endpoint, blocking for the response. */
    private fun httpPush(host: String, port: Int, body: String) {
        val url = URL("http://$host:$port/push")
        val conn = (url.openConnection() as HttpURLConnection).apply {
            requestMethod = "POST"
            connectTimeout = 5000
            readTimeout = 5000
            doOutput = true
            setFixedLengthStreamingMode(body.toByteArray(Charsets.UTF_8).size.toLong())
            setRequestProperty("Content-Type", "application/json")
        }
        try {
            conn.outputStream.use { os: OutputStream ->
                os.write(body.toByteArray(Charsets.UTF_8))
                os.flush()
            }
            val code = conn.responseCode
            assertTrue("POST /push failed: HTTP $code", code in 200..299)
        } finally {
            conn.disconnect()
        }
    }
}
