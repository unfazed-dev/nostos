package run.nostos.reactnative

import com.facebook.react.ReactPackage
import com.facebook.react.bridge.NativeModule
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.uimanager.ViewManager

/**
 * React package registering [NostosTurboModule] with the host RN app.
 *
 * A consumer app adds this to its `getPackages()` list (the standard RN
 * `MainApplication.getPackages()` override on Old Arch, or the autolinking
 * `ReactHost` / `DefaultReactNativeHost` path on New Arch). After registration,
 * `TurboModuleRegistry.getEnforcing<Spec>("NativeNostos")` from JS resolves to
 * the [NostosTurboModule] instance backing the `NativeNostos.ts` spec.
 *
 * This is a plain `ReactPackage` (not a `TurboReactPackage`) — the single
 * module is small + always registered, so the base interface is enough. RN's
 * bridge accepts base ReactPackages on both arches; the module is flagged as a
 * TurboModule by its `@ReactModule` annotation + the `ReactContextBaseJavaModule`
 * base class which implements `com.facebook.react.turbomodule.core.interfaces.TurboModule`
 * on New-Arch builds.
 */
class NostosPackage : ReactPackage {
    override fun createNativeModules(reactContext: ReactApplicationContext): List<NativeModule> =
        listOf(NostosTurboModule(reactContext))

    // No view managers — this package ships a TurboModule only.
    override fun createViewManagers(
        reactContext: ReactApplicationContext,
    ): List<ViewManager<*, *>> = emptyList()
}
