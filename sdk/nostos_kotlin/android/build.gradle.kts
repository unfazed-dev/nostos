// =============================================================================
// nostos-kotlin Android library — bundles `libnostos_kotlin.so` (arm64-v8a) +
// the UniFFI-generated Kotlin sources into a consumable `.aar`.
// -----------------------------------------------------------------------------
// Build shape mirrors Mozilla application-services' UniFFI-on-Android libraries:
// the .so lives in `src/main/jniLibs/<abi>/`, the generated Kotlin lives in
// `../kotlin-sources/uniffi/nostos_kotlin/`, and the runtime depends on JNA
// (UniFFI's Kotlin target dispatches FFI through `com.sun.jna.*`).
// =============================================================================
plugins {
    id("com.android.library") version "8.7.3"
    kotlin("android") version "1.9.24"
}

android {
    namespace = "run.nostos.sdk"
    compileSdk = 34

    defaultConfig {
        minSdk = 24

        // instrumented-test runner — needed for Tier-2 connectedDebugAndroidTest.
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        // Live-E2E port plumbing: the on-device `live_connect_push_echo_roundTrip`
        // test needs to know which port the host-side spine
        // (`target/debug/examples/e2e_server`) bound. The orchestrator
        // (`scripts/run-live-e2e.sh`) spawns the spine, captures its
        // `NOSTOS_E2E_PORT=`, and passes the value here via
        // `./gradlew connectedDebugAndroidTest -PnostosPort=<port>`. The test
        // reads it back via `InstrumentationRegistry.getArguments().getString("nostosPort")`.
        // "0" = unset → the live test self-skips (the offline test still runs).
        testInstrumentationRunnerArguments["nostosPort"] =
            (project.findProperty("nostosPort") ?: "0").toString()
    }

    sourceSets {
        getByName("main") {
            // Generated Kotlin from `uniffi-bindgen generate --language kotlin`
            // (run from the crate root, output dir `kotlin-sources/`).
            java.srcDirs("../kotlin-sources")
            // jniLibs default is `src/main/jniLibs` — explicit for clarity.
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    kotlinOptions {
        jvmTarget = "1.8"
    }

    // .so inside the .aar — `useLegacyPackaging = false` (default) keeps the
    // .so uncompressed + page-mapped directly from the apk. AGP 8.7+
    // page-aligns uncompressed jniLibs to 16KB when `useLegacyPackaging=false`,
    // which is what 16KB-page Android 15+ devices want.
    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }

    // ponytail: testOptions left default. A future on-device benchmark of the
    // sync engine would configure `testOptions { unitTests.isReturnDefaultValues = true }`
    // + add an `androidTest` micro-bench harness; out of scope for the
    // feasibility scaffold.
    testOptions {
        targetSdk = 34
    }
}

dependencies {
    // UniFFI 0.28's Kotlin runtime uses JNA for FFI dispatch. JNA 5.16.0 is
    // required on Android 15+ / 16 (API 35+) because its `libjnidispatch.so`
    // is built with 16KB-page-size ELF alignment; JNA ≤ 5.14.0's libjnidispatch.so
    // was 8KB-aligned and `dlopen` rejects it with "program alignment (8192)
    // cannot be smaller than system page size (16384)" on 16KB-page devices
    // (the running emulator-5554 is API 37 / 16KB-pages).
    implementation("net.java.dev.jna:jna:5.16.0@aar")
    // Also expose JNA to the instrumented-test classloader — the test apk
    // needs JNA's classes + libjnidispatch.so to be on ITS classpath, not
    // just on the library-under-test's. Without this, `connectedDebugAndroidTest`
    // fails with `UnsatisfiedLinkError: ...libjnidispatch.so not found`.
    androidTestImplementation("net.java.dev.jna:jna:5.16.0@aar")

    // Tier-2 instrumented test deps — connectedDebugAndroidTest runner.
    androidTestImplementation("androidx.test.ext:junit:1.1.5")
    androidTestImplementation("androidx.test:runner:1.5.2")
}
