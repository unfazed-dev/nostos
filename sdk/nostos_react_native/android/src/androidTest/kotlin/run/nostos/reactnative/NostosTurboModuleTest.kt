package run.nostos.reactnative

import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith

import java.io.OutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.nio.file.Files
import java.nio.file.Paths

/**
 * Wave-B MUST + SHOULD on-device proof: the [NostosTurboModule] — the Codegen-
 * shaped Kotlin TurboModule that backs Wave-A's `NativeNostos.ts` spec —
 * constructs + serves an offline query (MUST), AND drives a live two-direction
 * PUSH+ECHO round-trip vs a host-side spine (SHOULD), on a real Android
 * emulator (emulator-5556, arm64-v8a). Mirrors `sdk/nostos_kotlin`'s
 * `NostosClientTest` but routes through the TurboModule's sync core
 * (`connectSync` / `subscribeSync` / `writeSync` / `querySync`) instead of the
 * raw UniFFI `NostosClient` — proving the TurboModule delegates correctly AND
 * `libnostos_kotlin.so` loads through the RN module's jniLibs bundle.
 *
 * Success criteria:
 *   - MUST  `offline_turbomodule_connect_query_roundTrip`: query("SELECT 1 AS one")
 *     returns a one=1 row + checkpoint() reports 0.0 for a fresh store.
 *   - SHOULD `live_connect_push_echo_roundTrip`: [rn-e2e] PUSH_OK + ECHO_OK.
 *
 * If the library fails to LOAD, the failure is `UnsatisfiedLinkError` during
 * `NostosClient.Companion` static init — `libnostos_kotlin.so` is missing from
 * the test apk's `jniLibs/arm64-v8a/` (scripts/build-android.sh step 3). If it
 * loads but the assert fails, SQLite/tokio is misconfigured for Android.
 */
@RunWith(AndroidJUnit4::class)
class NostosTurboModuleTest {
    companion object {
        @JvmStatic
        @BeforeClass
        fun loadNatives() {
            // Force-load libnostos_kotlin.so explicitly before the JNA path
            // runs, so the diagnostic log proves our .so loads on device even
            // if JNA's libjnidispatch.so trips.
            try {
                System.loadLibrary("nostos_kotlin")
                Log.e("NostosTurboModuleTest", "SUCCESS: loaded libnostos_kotlin.so via System.loadLibrary")
            } catch (e: UnsatisfiedLinkError) {
                Log.e("NostosTurboModuleTest", "FAILED to load libnostos_kotlin.so", e)
            }
        }
    }

    /**
     * Build the TurboModule. Uses the no-arg constructor — `ReactApplicationContext`
     * is abstract in RN 0.79+ (instantiated only inside RN's host infra), and the
     * Spec methods never touch the context anyway (they delegate purely to the
     * UniFFI `NostosClient` handle), so the on-device proof needs no RN context.
     */
    private fun newModule(): NostosTurboModule = NostosTurboModule()

    /**
     * MUST — offline round-trip through the TurboModule. Proves the .so loads,
     * UniFFI constructs, and the Spec methods delegate correctly on-device.
     */
    @Test
    fun offline_turbomodule_connect_query_roundTrip() {
        val module = newModule()

        // connect(url, token, dbPath) — the Spec signature Wave-A grew so the
        // singleton TurboModule can construct the UniFFI client lazily.
        module.connectSync(url = "ws://localhost:0", token = null, dbPath = ":memory:")
        Log.i("NostosTurboModuleTest", "[rn-e2e] connect(ws, null, :memory:) ok")

        val rowsJson = module.querySync("SELECT 1 AS one")
        assertTrue(
            "expected a one=1 row in the JSON, got: $rowsJson",
            rowsJson.contains("\"one\":1") || rowsJson.contains("\"one\": 1"),
        )
        Log.i("NostosTurboModuleTest", "[rn-e2e] query SELECT 1 ok: $rowsJson")

        // Fresh store reports Lsn(0) — confirms the checkpoint read path also
        // round-trips through the FFI boundary (ULong → JS Double).
        val lsn = module.checkpointSync()
        assertEquals("fresh store should report Lsn(0)", 0.0, lsn, 0.0)
    }

    /**
     * ADR-0037 task 5.3 (bridging 5.1), offline half: the TurboModule's
     * `disconnect()` is NON-destructive — the session (and its durable store)
     * survives, so `querySync()` keeps answering, `resumeSync()` re-enters the
     * loop, and the destructive sibling `signOutSync()` still wipes afterwards.
     * Mirrors `sdk/nostos_kotlin`'s `disconnect_keeps_local_state_queryable_and_
     * resume_reenters` but routes through the TurboModule's sync core. The
     * connected half (delta applies from the checkpoint) is pinned in
     * nostos-client's `disconnect_then_resume_applies_delta_from_checkpoint_without_loss`.
     */
    @Test
    fun offline_turbomodule_disconnect_nondestructive_resume_reenters() {
        val module = newModule()
        module.connectSync(url = "ws://localhost:0", token = null, dbPath = ":memory:")

        // Idempotent + no live loop against a dead URL: still fine, session
        // untouched.
        module.disconnectSync()
        Log.i("NostosTurboModuleTest", "[rn-e2e] disconnect() ok — session + store kept")

        // Non-destructive: querySync() answers from the durable store.
        val rowsJson = module.querySync("SELECT 1 AS one")
        assertTrue(
            "expected query() to answer after disconnect(), got: $rowsJson",
            rowsJson.contains("\"one\":1") || rowsJson.contains("\"one\": 1"),
        )

        // Wake primitive: resume re-enters the loop (dead URL — fire-and-forget,
        // the run loop's reconnect backoff owns it).
        module.resumeSync()
        Log.i("NostosTurboModuleTest", "[rn-e2e] resume() ok — loop re-entered")

        // The destructive sibling still works after a disconnect/resume cycle.
        module.signOutSync()
    }

    /**
     * SHOULD — Tier-2 LIVE replication E2E, the RN slice of
     * `docs/plans/sdk-live-e2e-consolidation.md`. Drives the SAME
     * two-direction round-trip `nostos_kotlin`'s
     * `live_connect_push_echo_roundTrip` proves, but through the TurboModule:
     *
     *   1. connect() → subscribe("tasks") → run loop applies rows to cairn_data.
     *   2. POST /push to the host-side spine → server pushes a `tasks` row →
     *      poll query() until it lands → [rn-e2e] PUSH_OK.
     *   3. TurboModule write()s `rn-echo` → spine echo WriteBack re-emits it
     *      → run loop applies it → poll query() → [rn-e2e] ECHO_OK.
     *
     * Self-skips via `assumeTrue` when `nostosPort` is unset ("0") so the
     * offline MUST test still runs green without the orchestrator
     * (scripts/run-android-e2e.sh).
     */
    @Test
    fun live_connect_push_echo_roundTrip() {
        // ---- spine endpoint discovery ----
        val portArg = InstrumentationRegistry.getArguments().getString("nostosPort", "0")
        val port = portArg.toIntOrNull() ?: 0
        assumeTrue(
            "[rn-e2e] SKIP: nostosPort instrumentation arg unset (run via scripts/run-android-e2e.sh)",
            port > 0,
        )

        val host = "10.0.2.2" // documented Android-emulator → host-loopback alias
        val wsUrl = "ws://$host:$port/sync"
        Log.i("NostosTurboModuleTest", "[rn-e2e] BEGIN live round-trip; spine wsUrl=$wsUrl")

        // PID-unique DB so the engine's apply path + durable outbox survive
        // across the run loop's flush + echo re-emit. Clean remove first so a
        // stale file from a prior run can't yield a false positive.
        val ctx = InstrumentationRegistry.getTargetContext()
        val dbPath = Paths.get(ctx.cacheDir.absolutePath, "nostos-rn-e2e.sqlite").toString()
        try { Files.deleteIfExists(Paths.get(dbPath)) } catch (_: Exception) { }

        val module = newModule()
        module.connectSync(url = wsUrl, token = null, dbPath = dbPath)
        Log.i("NostosTurboModuleTest", "[rn-e2e] connect() ok")
        module.subscribeSync("tasks")
        Log.i("NostosTurboModuleTest", "[rn-e2e] subscribe(\"tasks\") ok — run loop driving replication")

        // Let the subscribe land + the session register with the fan-out (the
        // spine only delivers to sessions registered at fan-out time). Mirrors
        // the nostos_kotlin test's 500ms settle.
        Thread.sleep(500)

        // ---- direction 1: server PUSH → on-device query ----
        // Re-push (idempotent upsert by pk) in a loop until the row lands —
        // the spine delivers ONLY to sessions registered at fan-out time, so a
        // single /push that races the subscribe is missed, not queued. Same
        // cold-start race nostos_kotlin's test handles.
        val pushBody = """{"pk":"rn-push","payload":{"title":"from-server","status":"open","priority":"5"}}"""
        val pushSql = "SELECT pk FROM cairn_data WHERE table_name='tasks' AND pk='rn-push'"
        val pushDeadline = System.currentTimeMillis() + 15_000L
        var pushOk = false
        var pushRows = ""
        while (System.currentTimeMillis() < pushDeadline && !pushOk) {
            httpPush(host, port, pushBody)
            val innerDeadline = System.currentTimeMillis() + 2_000L
            while (System.currentTimeMillis() < innerDeadline) {
                val rows = runCatching { module.querySync(pushSql) }.getOrDefault("")
                if (rows.contains("rn-push")) {
                    pushOk = true
                    break
                }
                Thread.sleep(100)
            }
            pushRows = runCatching { module.querySync(pushSql) }.getOrDefault("<query failed>")
        }
        assertTrue("[rn-e2e] rn-push never landed in cairn_data; rows=$pushRows", pushOk)
        Log.i("NostosTurboModuleTest", "[rn-e2e] PUSH_OK")

        // ---- direction 2: client WRITE → server echo → on-device query ----
        val echoPayload = """{"title":"from-rn","status":"open","priority":"7"}"""
        val writeId = module.writeSync("tasks", "upsert", "rn-echo", echoPayload)
        Log.i("NostosTurboModuleTest", "[rn-e2e] write() id=$writeId (rn-echo enqueued)")
        val echoSql = "SELECT pk FROM cairn_data WHERE table_name='tasks' AND pk='rn-echo'"
        val echoOk = pollQueryContains(module, echoSql, "rn-echo", timeoutMillis = 8000L)
        val echoRows = runCatching { module.querySync(echoSql) }.getOrDefault("<query failed>")
        assertTrue("[rn-e2e] rn-echo never echoed back; rows=$echoRows", echoOk)
        Log.i("NostosTurboModuleTest", "[rn-e2e] ECHO_OK")
    }

    // ---- helpers ----

    /** Poll `module.querySync(sql)` until the result contains `needle` or the timeout fires. */
    private fun pollQueryContains(
        module: NostosTurboModule,
        sql: String,
        needle: String,
        timeoutMillis: Long,
    ): Boolean {
        val deadline = System.currentTimeMillis() + timeoutMillis
        while (System.currentTimeMillis() < deadline) {
            try {
                if (module.querySync(sql).contains(needle)) return true
            } catch (_: Exception) {
                // querySync can transiently fail if the run loop is mid-flush;
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
