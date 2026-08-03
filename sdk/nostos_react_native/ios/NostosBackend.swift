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
        // ADR-0024 reactive push pump — deferred on iOS (request/response +
        // signOut/setToken ship first; the plan's out-of-scope note).
        reject("NostosWatchPending", "watchChanges reactive push not yet supported on iOS.", nil)
    }

    @objc(bridgeUnwatch:resolve:reject:)
    public func unwatchChanges(table: String, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        resolve(nil) // nothing started; idempotent no-op
    }

    @objc(bridgeSetToken:resolve:reject:)
    public func setToken(token: String?, resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        // Infallible locally (interior-mutable token cell); no-op if pre-connect.
        backing?.setToken(token: token)
        resolve(nil)
    }

    @objc(bridgeSignOut:reject:)
    public func signOut(resolve: RCTPromiseResolveBlock!, reject: RCTPromiseRejectBlock!) {
        do { try backing?.signOut(); resolve(nil) }
        catch { reject("NostosSignOutError", (error as NSError).localizedDescription, error as NSError) }
    }
}
