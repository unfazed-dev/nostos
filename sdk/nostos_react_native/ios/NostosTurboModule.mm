// NostosTurboModule.mm — RN Codegen TurboModule for @nostos-sync/react-native (iOS).
//
// Conforms to the @react-native/codegen-generated `NativeNostosSpec` Obj-C
// protocol (build/generated/.../NativeNostos/NativeNostos.h) and forwards each
// method to the Swift `NostosBackend`, which reuses the UniFFI-generated
// nostos_swift.swift `NostosClient` — the SAME nostos_client::SyncClient
// <SqliteStorage> every other SDK uses. Mirrors the Android Kotlin module
// (sdk/nostos_react_native/android/.../NostosTurboModule.kt).
//
// Why .mm (Obj-C++) + a Swift backend: the generated NativeNostosSpec header is
// Obj-C++ only (it imports <optional> + emits a C++ JSI class with a
// `#error ... must be .mm` guard), so a Swift class cannot import it directly.
// This .mm owns the protocol conformance; the Swift backend owns the
// nostos_swift delegation (reusing UniFFI's marshalling — NO hand-rolled
// RustBuffer FFI, the failure-prone part of the raw C ABI). See
// docs/plans/nostos-rn-ios-turbomodule-2026-08-03.md.

#import <Foundation/Foundation.h>
#import <NativeNostos/NativeNostos.h>
#import <React/RCTBridgeModule.h>
#import <React/RCTLog.h>

// The auto-generated Obj-C interface for the pod's Swift code (NostosBackend).
// NostosBackend is `public @objc` so it's emitted into this header in the
// framework build (RN 0.86 builds NostosReactNative as a framework).
#import "NostosReactNative-Swift.h"

@interface NostosTurboModule : NSObject <NativeNostosSpec>
@end

@interface NostosTurboModule () {
  NostosBackend *_backend;
}
@end

@implementation NostosTurboModule

// JS looks up "NativeNostos" (TurboModuleRegistry.getEnforcing("NativeNostos"),
// matching codegenConfig.name + NativeNostos.ts). The class name is arbitrary.
RCT_EXPORT_MODULE(NativeNostos)

// Run all Spec methods on a SERIAL BACKGROUND queue, NOT the JS thread. The
// nostos_swift UniFFI methods block on an owned tokio runtime (the ponytail:
// block-on-owned-runtime decision in nostos_swift/src/lib.rs) — blocking the JS
// thread stalls React. Android's bridge runs @ReactMethod on a background
// thread by default; this methodQueue is the iOS equivalent. Mirrors the
// Kotlin module's execution model (its e2e is 10/10 green).
- (dispatch_queue_t)methodQueue {
  static dispatch_queue_t queue;
  static dispatch_once_t onceToken;
  dispatch_once(&onceToken, ^{
    queue = dispatch_queue_create("run.nostos.turbomodule", DISPATCH_QUEUE_SERIAL);
  });
  return queue;
}

- (NostosBackend *)backend {
  if (!_backend) {
    _backend = [NostosBackend new];
  }
  return _backend;
}

- (void)connect:(NSString *)url
          token:(NSString *_Nullable)token
         dbPath:(NSString *)dbPath
        resolve:(RCTPromiseResolveBlock)resolve
         reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeConnect:url token:token dbPath:dbPath resolve:resolve reject:reject];
}

- (void)subscribe:(NSString *)table
          resolve:(RCTPromiseResolveBlock)resolve
           reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeSubscribe:table resolve:resolve reject:reject];
}

- (void)write:(NSString *)table
           op:(NSString *)op
           pk:(NSString *)pk
  payloadJson:(NSString *_Nullable)payloadJson
      resolve:(RCTPromiseResolveBlock)resolve
       reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeWrite:table op:op pk:pk payloadJson:payloadJson resolve:resolve reject:reject];
}

- (void)query:(NSString *)sql
      resolve:(RCTPromiseResolveBlock)resolve
       reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeQuery:sql resolve:resolve reject:reject];
}

- (void)checkpoint:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeCheckpoint:resolve reject:reject];
}

- (void)watchChanges:(NSString *)table
          onSnapshot:(RCTResponseSenderBlock)onSnapshot
             resolve:(RCTPromiseResolveBlock)resolve
              reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeWatch:table onSnapshot:onSnapshot resolve:resolve reject:reject];
}

- (void)unwatchChanges:(NSString *)table
               resolve:(RCTPromiseResolveBlock)resolve
                reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeUnwatch:table resolve:resolve reject:reject];
}

- (void)setToken:(NSString *_Nullable)token
         resolve:(RCTPromiseResolveBlock)resolve
          reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeSetToken:token resolve:resolve reject:reject];
}

- (void)signOut:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self.backend bridgeSignOut:resolve reject:reject];
}

// THE SEAM: opt this module into its codegen SpecJSI. Without this, the
// TurboModuleManager wraps NostosTurboModule generically — getEnforcing returns
// a method-less object (no Spec methodMap_), so `NativeNostos.connect(...)` is
// undefined in JS. Returning NativeNostosSpecJSI(params) (whose methodMap_
// routes connect/subscribe/.../signOut to the Obj-C methods above) is how every
// codegen TurboModule exposes its Spec surface — see react-native-safe-area-
// context's RNCSafeAreaContext.mm:78-81 for the canonical pattern.
#ifdef RCT_NEW_ARCH_ENABLED
- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  return std::make_shared<facebook::react::NativeNostosSpecJSI>(params);
}
#endif

@end
