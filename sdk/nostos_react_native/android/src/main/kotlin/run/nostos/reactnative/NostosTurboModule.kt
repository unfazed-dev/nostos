package run.nostos.reactnative

import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactMethod
import uniffi.nostos_kotlin.NostosClient

/**
 * Concrete [NativeNostosSpec] implementation — the Wave-B Kotlin TurboModule
 * that backs Wave-A's `NativeNostos.ts` JS spec.
 *
 * Wraps the UniFFI-generated [NostosClient] (`sdk/nostos_kotlin`'s
 * `libnostos_kotlin.so` + `uniffi.nostos_kotlin.NostosClient` Kotlin sources),
 * reusing it WHOLESALE — no Rust forked or rebuilt. The UniFFI methods are
 * synchronous (they block on an owned tokio runtime inside `nostos_kotlin`),
 * so each `@ReactMethod` resolves its [Promise] inline on the calling thread
 * before returning. This matches the `ponytail:` block-on-owned-runtime
 * decision documented in `sdk/nostos_swift/src/lib.rs` and lifts the SAME shape
 * PowerSync's RN SDK validated (native sync client reachable from JS).
 *
 * TurboModules are singletons per React instance (instantiated by RN with a
 * no-arg equivalent — the [ReactApplicationContext] constructor here). There
 * is no JS-visible constructor to pass `(url, token, dbPath)` through, so the
 * backing UniFFI client is constructed lazily on the first [connect] call and
 * the JS facade (`NostosClient.ts`) threads its captured config through
 * `NativeNostos.connect(url, token, dbPath)`.
 *
 * ULong → JS mapping: UniFFI returns `ULong` LSNs for write/checkpoint. JS
 * numbers are IEEE-754 doubles (53 bits of mantissa), and durable LSNs fit
 * there for any realistic session.
 * ponytail: CEILING — a sync exceeding 2^53 rows would lose precision. Upgrade
 * path: return the LSN as a JSON string + have the JS facade parse to BigInt
 * (Wave C, if ever needed).
 */
class NostosTurboModule :
    NativeNostosSpec {
    /** RN-bridge-facing constructor — used by the module registry in a host app. */
    constructor(reactContext: ReactApplicationContext) : super(reactContext)

    /**
     * Test-facing no-arg constructor — `ReactApplicationContext` is abstract in
     * RN 0.79+, so instrumented tests build the module without one. The Spec
     * methods never touch the context (they delegate to the UniFFI handle), so
     * a null context is fine for the on-device round-trip proof.
     */
    constructor() : super()

    // Volatile: constructed inside connect() (called from the JS thread / the
    // instrumented test's main thread), read by every subsequent method. The
    // RN contract serializes TurboModule calls, so the volatile read is the
    // correct cheap published-read here — no lock needed.
    @Volatile private var backing: NostosClient? = null

    // ---- public synchronous core ------------------------------------------
    // Exposed (non-@ReactMethod) so the instrumented test
    // (NostosTurboModuleTest) can drive the round-trip WITHOUT a JS runtime /
    // Promise machinery — proving the .so + UniFFI + TurboModule wiring on
    // device. The @ReactMethod Promise wrappers below delegate here.

    fun connectSync(url: String, token: String?, dbPath: String) {
        if (backing == null) {
            backing = NostosClient(url = url, token = token, dbPath = dbPath)
        }
        client().connect()
    }

    fun subscribeSync(table: String) {
        client().subscribe(table)
    }

    fun writeSync(table: String, op: String, pk: String, payloadJson: String?): Double =
        client().write(table, op, pk, payloadJson).toDouble()

    fun querySync(sql: String): String = client().query(sql)

    fun checkpointSync(): Double = client().checkpoint().toDouble()

    /**
     * Hot-swap the bearer on the live session (ADR-0029 #3). Delegates to the
     * UniFFI `setToken(token: String?)` — the interior-mutable token cell the
     * reconnect loop reads on its next attempt. `null` clears (anonymous).
     * Callable before [connect] AND on a live session (the UniFFI method is
     * infallible in that it does not tear anything down; it throws `NostosError`
     * only if the underlying client rejects the swap).
     */
    fun setTokenSync(token: String?) {
        client().setToken(token)
    }

    /**
     * Sign out (ADR-0029): abort the run loop, await quiescence, wipe local
     * state (rows + checkpoint + epoch + outbox + dead-letter), drop the
     * session, and clear the token. Delegates to UniFFI `signOut()`.
     *
     * `sign_out(&self)` takes `&self` (not `self`), so the [backing] handle
     * REMAINS valid after this returns — a subsequent [connect] re-establishes
     * the session on the SAME handle. The JS facade drops its own bookkeeping
     * on signOut; nothing here nulls [backing] (connect's `if (backing == null)`
     * guard intentionally reuses a wiped handle).
     */
    fun signOutSync() {
        client().signOut()
    }

    // ---- @ReactMethod Promise wrappers (the Spec surface JS calls) --------

    @ReactMethod
    override fun connect(url: String, token: String?, dbPath: String, promise: Promise) {
        try {
            connectSync(url, token, dbPath)
            promise.resolve(null)
        } catch (t: Throwable) {
            promise.reject("NostosConnectError", t)
        }
    }

    @ReactMethod
    override fun subscribe(table: String, promise: Promise) {
        try {
            subscribeSync(table)
            promise.resolve(null)
        } catch (t: Throwable) {
            promise.reject("NostosSubscribeError", t)
        }
    }

    @ReactMethod
    override fun write(
        table: String,
        op: String,
        pk: String,
        payloadJson: String?,
        promise: Promise,
    ) {
        try {
            promise.resolve(writeSync(table, op, pk, payloadJson))
        } catch (t: Throwable) {
            promise.reject("NostosWriteError", t)
        }
    }

    @ReactMethod
    override fun query(sql: String, promise: Promise) {
        try {
            promise.resolve(querySync(sql))
        } catch (t: Throwable) {
            promise.reject("NostosQueryError", t)
        }
    }

    @ReactMethod
    override fun checkpoint(promise: Promise) {
        try {
            promise.resolve(checkpointSync())
        } catch (t: Throwable) {
            promise.reject("NostosCheckpointError", t)
        }
    }

    @ReactMethod
    override fun setToken(token: String?, promise: Promise) {
        try {
            setTokenSync(token)
            promise.resolve(null)
        } catch (t: Throwable) {
            promise.reject("NostosSetTokenError", t)
        }
    }

    @ReactMethod
    override fun signOut(promise: Promise) {
        try {
            signOutSync()
            promise.resolve(null)
        } catch (t: Throwable) {
            promise.reject("NostosSignOutError", t)
        }
    }

    private fun client(): NostosClient = backing
        ?: throw IllegalStateException(
            "NostosTurboModule: connect(url, token, dbPath) must be called before any other method",
        )
}
