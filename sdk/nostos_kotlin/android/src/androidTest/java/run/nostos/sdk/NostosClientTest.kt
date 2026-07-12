package run.nostos.sdk

import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith

import uniffi.nostos_kotlin.NostosClient

/**
 * Tier-2 emulator-E2E proof: the SAME `SyncClient<SqliteStorage>` the sibling
 * SDKs drive constructs + serves an offline query through the UniFFI
 * `NostosClient` shape, on a real Android emulator (emulator-5554, arm64-v8a,
 * API 37). Mirrors `sdk/nostos_swift/src/lib.rs::tests::nostos_client_offline_connect_query_round_trip`
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
}
