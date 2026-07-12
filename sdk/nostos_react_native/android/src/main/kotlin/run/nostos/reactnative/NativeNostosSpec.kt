package run.nostos.reactnative

import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.module.annotations.ReactModule

/**
 * The Codegen-equivalent Spec for the `NativeNostos` TurboModule.
 *
 * `@react-native/codegen` would emit this abstract class from
 * `src/NativeNostos.ts` (codegenConfig `name = "NativeNostos"`) at native-build
 * time inside a host RN app. It is hand-mirrored here verbatim so this
 * standalone library module builds + unit-tests WITHOUT needing the codegen
 * gradle task to run: the concrete override in [NostosTurboModule] is what
 * RN's runtime `@ReactMethod` annotation scanner reads, and the
 * `@ReactModule(name = ...)` on this class binds the JS-side
 * `TurboModuleRegistry.getEnforcing("NativeNostos")` lookup to the concrete
 * implementation.
 *
 * Method-by-method (spec → UniFFI `uniffi.nostos_kotlin.NostosClient`):
 *   connect(url, token, dbPath) → NostosClient(url, token, dbPath) + .connect()
 *   subscribe(table)            → .subscribe(table)
 *   write(table, op, pk, pj)    → .write(table, op, pk, payloadJson)  (ULong → JS Double)
 *   query(sql)                  → .query(sql)                          (JSON-rows String)
 *   checkpoint()                → .checkpoint()                        (ULong → JS Double)
 *
 * `payloadJson: String?` mirrors UniFFI's `Option<String>`: `null` = None
 * (delete shape — no row image), a JSON string = Some(...). The Kotlin `?`
 * matches the TS spec's `string | null`.
 */
@ReactModule(name = NativeNostosSpec.NAME)
abstract class NativeNostosSpec :
    ReactContextBaseJavaModule {
    /**
     * RN-bridge-facing constructor — the one RN's module registry uses to
     * instantiate the TurboModule inside a host app.
     */
    constructor(reactContext: ReactApplicationContext) : super(reactContext)

    /**
     * Test-facing no-arg constructor. `ReactApplicationContext` is abstract in
     * RN 0.79+ (instantiated only inside the React host infra), so on-device
     * instrumented tests construct the module without one. Safe because the
     * Spec methods never touch `getReactApplicationContext()` — they delegate
     * purely to the UniFFI `NostosClient` handle.
     */
    constructor() : super()

    override fun getName(): String = NAME

    abstract fun connect(url: String, token: String?, dbPath: String, promise: Promise)
    abstract fun subscribe(table: String, promise: Promise)
    abstract fun write(
        table: String,
        op: String,
        pk: String,
        payloadJson: String?,
        promise: Promise,
    )
    abstract fun query(sql: String, promise: Promise)
    abstract fun checkpoint(promise: Promise)

    companion object {
        const val NAME = "NativeNostos"
    }
}
