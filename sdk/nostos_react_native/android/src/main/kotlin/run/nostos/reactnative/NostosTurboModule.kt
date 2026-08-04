package run.nostos.reactnative

import com.facebook.react.bridge.Callback
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactMethod
import uniffi.nostos_kotlin.NostosClient
import uniffi.nostos_kotlin.SnapshotSink
import java.util.concurrent.ConcurrentHashMap

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

    // Per-table retained snapshot sinks (the Kotlin mirror of iOS
    // `NostosBackend.sinks`). The sink object is KEPT here until the session
    // ends even after `unwatchChanges` nil's its callback: UniFFI's handle map
    // + the Rust pump hold a handle into it (nostos_kotlin has no `stop_watch`
    // — the pump is tied to the session), so dropping it mid-session would
    // leave a dangling handle. `ConcurrentHashMap` because `watchChanges` /
    // `unwatchChanges` may be invoked from the RN bridge background thread
    // while a tick is being delivered on the nostos tokio worker.
    private val sinks = ConcurrentHashMap<String, NostosSnapshotSink>()

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
        // The session — and every watch pump tied to it — is gone; drop the
        // retained sinks (mirror of iOS `NostosBackend.signOut` removeAll).
        sinks.clear()
    }

    /**
     * Start the native push pump for `table` (the Kotlin mirror of iOS
     * `NostosBackend.watchChanges`). Load-bearing ordering (nostos_kotlin
     * `watch()`, lib.rs): `subscribe()` runs BEFORE the first snapshot read so
     * the initial snapshot can't miss rows — `subscribe()` is idempotent, so a
     * prior caller-issued `subscribe(table)` is harmless. `watch()` then emits
     * the initial snapshot to `sink` synchronously here, and again after every
     * applied change (remote apply or local write) — full snapshot per tick.
     *
     * `onSnapshot` is an RN [Callback] (the Codegen mapping of a JS
     * `(rowsJson) => void` param); it self-marshals onto the JS thread, so the
     * nostos tokio worker may invoke it directly (the Android analogue of iOS's
     * self-marshaling `RCTResponseSenderBlock`).
     */
    fun watchChangesSync(table: String, onSnapshot: Callback) {
        client().subscribe(table)
        val sink = NostosSnapshotSink(onSnapshot)
        sinks[table] = sink
        // watch() emits the initial snapshot to `sink` synchronously here.
        client().watch(table, sink)
    }

    /**
     * Stop DELIVERY to JS for `table` (the Kotlin mirror of iOS
     * `NostosBackend.unwatchChanges`). Nil's the retained callback so further
     * ticks are no-ops; the sink object stays in [sinks] until the session
     * ends (UniFFI's handle map holds a reference — no `stop_watch` binding).
     * Idempotent.
     */
    fun unwatchChangesSync(table: String) {
        sinks[table]?.release()
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

    @ReactMethod
    override fun watchChanges(table: String, onSnapshot: Callback, promise: Promise) {
        try {
            watchChangesSync(table, onSnapshot)
            promise.resolve(null)
        } catch (t: Throwable) {
            promise.reject("NostosWatchError", t)
        }
    }

    @ReactMethod
    override fun unwatchChanges(table: String, promise: Promise) {
        try {
            unwatchChangesSync(table)
            promise.resolve(null)
        } catch (t: Throwable) {
            promise.reject("NostosUnwatchError", t)
        }
    }

    private fun client(): NostosClient = backing
        ?: throw IllegalStateException(
            "NostosTurboModule: connect(url, token, dbPath) must be called before any other method",
        )
}

/**
 * Bridges the UniFFI `SnapshotSink` callback (synchronous, invoked from the
 * nostos tokio worker thread) to a retained RN JS [Callback] — the Kotlin
 * mirror of iOS's `NostosSnapshotSink`.
 *
 * `Callback.invoke(...)` marshals onto the JS thread (the catalyst instance
 * posts the args), so it is safe to call from the runtime worker — the Android
 * analogue of iOS's self-marshaling `RCTResponseSenderBlock`, and the same
 * shape `nostos_node`'s napi `ThreadsafeFunction` and `nostos_kotlin`'s
 * `SnapshotSink` validate. [release] nil's the ref so further ticks are no-ops
 * (`unwatchChanges`); the object itself is retained by
 * [NostosTurboModule.sinks] until the session ends (UniFFI's handle map holds a
 * reference — the no-`stop_watch` binding floor).
 *
 * `@Volatile` on [callback] gives the release()/onSnapshot() handoff the same
 * visibility guarantee iOS's `NSLock` provides (read-then-invoke outside the
 * lock; a tick in flight during release may fire once more — benign, full
 * snapshot per tick is self-healing).
 */
private class NostosSnapshotSink(callback: Callback) : SnapshotSink {
    @Volatile private var callback: Callback? = callback

    override fun onSnapshot(json: String) {
        // invoke(json) — Callback takes the JS-side args as varargs; the
        // facade's bridge expects a single `rowsJson: string` arg.
        callback?.invoke(json)
    }

    /** Idempotent: nil the retained callback so subsequent ticks are no-ops. */
    fun release() {
        callback = null
    }
}
