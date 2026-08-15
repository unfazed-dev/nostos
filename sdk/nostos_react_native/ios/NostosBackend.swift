// NostosBackend.swift — iOS TurboModule nostos_swift delegation layer.
//
// Reuses the UniFFI-generated `nostos_swift.swift` (compiled into this pod
// target alongside this file) so NO hand-written RustBuffer FFI is needed.
// The Obj-C++ shell NostosTurboModule.mm forwards each NativeNostosSpec method
// here via the explicit @objc(selector) names. Mirrors the Android Kotlin
// module's lazy-backing + Promise shape.
//
// `public`: NostosReactNative builds as a FRAMEWORK (RN 0.86), where only
// public @objc declarations are emitted into the generated Swift header that
// NostosTurboModule.mm imports — internal classes stay hidden from Obj-C.
//
// ponytail: backing is constructed lazily in bridgeConnect (TurboModules are
// singletons with a no-arg constructor — there is no JS surface to pass
// (url, token, dbPath) through at construction, per NativeNostos.ts). Pre-
// connect setToken is a no-op when backing is nil (the JS facade connects
// first by contract; matches the Kotlin module's lazy shape).

import Foundation
import React

@objc public final class NostosBackend: NSObject {
    private var backing: NostosClient?
    // Per-table watch sinks, RETAINED for the session lifetime. UniFFI holds a
    // handle back into each Swift sink object (the Rust pump calls onSnapshot),
    // so a sink must outlive the pump — it can't be released at unwatch or the
    // handle dangles. unwatch nil's the callback (delivery stops) but leaves
    // the sink here until signOut/deinit. Binding floor: no stop_watch exists.
    private var sinks: [String: NostosSnapshotSink] = [:]
    private let sinksLock = NSLock()

    @objc(bridgeConnect:token:dbPath:resolve:reject:)
    public func connect(url: String, token: String?, dbPath: String,
                 resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do {
            if backing == nil {
                backing = try NostosClient(url: url, token: token, dbPath: dbPath)
            }
            try backing?.connect()
            resolve(nil)
        } catch {
            reject("NostosConnectError", (error as NSError).localizedDescription, error as NSError)
        }
    }

    @objc(bridgeSubscribe:resolve:reject:)
    public func subscribe(table: String, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do { try backing?.subscribe(table: table); resolve(nil) }
        catch { reject("NostosSubscribeError", (error as NSError).localizedDescription, error as NSError) }
    }

    @objc(bridgeWrite:op:pk:payloadJson:resolve:reject:)
    public func write(table: String, op: String, pk: String, payloadJson: String?,
               resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do {
            // UInt64 LSN → NSNumber (JS double, 53-bit mantissa — fine for any
            // realistic session; the Kotlin module's ponytail: ceiling applies).
            let id = try backing?.write(table: table, op: op, pk: pk, payloadJson: payloadJson) ?? 0
            resolve(id)
        } catch { reject("NostosWriteError", (error as NSError).localizedDescription, error as NSError) }
    }

    @objc(bridgeQuery:resolve:reject:)
    public func query(sql: String, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do { resolve(try backing?.query(sql: sql) ?? "[]") }
        catch { reject("NostosQueryError", (error as NSError).localizedDescription, error as NSError) }
    }

    @objc(bridgeCheckpoint:reject:)
    public func checkpoint(resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do { resolve(try backing?.checkpoint() ?? 0) }
        catch { reject("NostosCheckpointError", (error as NSError).localizedDescription, error as NSError) }
    }

    @objc(bridgeWatch:onSnapshot:resolve:reject:)
    public func watchChanges(table: String, onSnapshot: RCTResponseSenderBlock!,
                      resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do {
            // Load-bearing ordering (sdk/nostos_swift): subscribe BEFORE the
            // first snapshot read, or the initial snapshot can miss rows.
            try backing?.subscribe(table: table)
            let sink = NostosSnapshotSink(onSnapshot: onSnapshot)
            sinksLock.lock(); sinks[table] = sink; sinksLock.unlock()
            // watch() emits the initial snapshot to `sink` synchronously here
            // (a full-table read of the local store) — so even against a dead
            // endpoint the JS callback fires once with the current rows.
            try backing?.watch(table: table, sink: sink)
            resolve(nil)
        } catch {
            reject("NostosWatchError", (error as NSError).localizedDescription, error as NSError)
        }
    }

    @objc(bridgeUnwatch:resolve:reject:)
    public func unwatchChanges(table: String, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        // nil the callback (delivery to JS stops); RETAIN the sink (UniFFI
        // holds a handle into it — see `sinks` comment). Binding floor: no
        // stop_watch, so the Rust pump itself runs until the session ends.
        sinksLock.lock(); sinks[table]?.release(); sinksLock.unlock()
        resolve(nil)
    }

    @objc(bridgeResolveDbPath:resolve:reject:)
    public func resolveDbPath(name: String, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        // JS has no FS access on RN; hand it a writable per-app path. The same
        // `name` across a signOut-and-reopen resolves to the SAME file, so the
        // cross-reopen wipe is observable (a :memory: store can't prove it).
        let dir = NSTemporaryDirectory()
        resolve((dir as NSString).appendingPathComponent(name))
    }

    @objc(bridgeSetToken:resolve:reject:)
    public func setToken(token: String?, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        // Infallible locally (interior-mutable token cell); no-op if pre-connect.
        backing?.setToken(token: token)
        resolve(nil)
    }

    @objc(bridgeDisconnect:reject:)
    public func disconnect(resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        // NON-destructive pause (ADR-0037 task 5.1) — session, store, and token
        // survive; the run loop gates closed + quiesces. Idempotent, no-op pre-connect.
        do { try backing?.disconnect(); resolve(nil) }
        catch { reject("NostosDisconnectError", (error as NSError).localizedDescription, error as NSError) }
    }

    @objc(bridgeResume:reject:)
    public func resume(resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        // The push wake primitive (ADR-0037 task 5.1): re-open the loop after
        // disconnect(); resume_lsn re-seeds from the durable checkpoint. Throws
        // (→ rejection) if called before connect — the UniFFI contract.
        do { try backing?.resume(); resolve(nil) }
        catch { reject("NostosResumeError", (error as NSError).localizedDescription, error as NSError) }
    }

    @objc(bridgeSignOut:reject:)
    public func signOut(resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do {
            try backing?.signOut()
            // Drop retained sinks (the session — and its pumps — is gone).
            sinksLock.lock(); sinks.removeAll(); sinksLock.unlock()
            resolve(nil)
        }
        catch { reject("NostosSignOutError", (error as NSError).localizedDescription, error as NSError) }
    }
}

/// Bridges the UniFFI `SnapshotSink` callback (synchronous, invoked from the
/// nostos tokio worker thread) to a retained JS `RCTResponseSenderBlock`.
/// `onSnapshot` is called by Rust on the runtime thread; `RCTResponseSenderBlock`
/// self-marshals to the JS thread (via the JSCallInvoker), so this is safe to
/// invoke off the JS thread — verified on the iOS sim. `release()` nil's the
/// block under the lock so further ticks are no-ops (unwatch); the object
/// itself is retained by `NostosBackend.sinks` until session end.
final class NostosSnapshotSink: SnapshotSink, @unchecked Sendable {
    private let lock = NSLock()
    private var callback: RCTResponseSenderBlock?

    init(onSnapshot: @escaping RCTResponseSenderBlock) {
        self.callback = onSnapshot
    }

    func onSnapshot(json: String) {
        lock.lock()
        let cb = callback
        lock.unlock()
        // RCTResponseSenderBlock takes an NSArray of the JS-side args.
        cb?([json])
    }

    /// Idempotent: nil the retained callback so subsequent ticks are no-ops.
    func release() {
        lock.lock(); callback = nil; lock.unlock()
    }
}
