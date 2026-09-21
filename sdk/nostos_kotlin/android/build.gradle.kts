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
    `maven-publish`
    signing
}

// Coordinates. Both overridable from the command line / gradle.properties so
// the namespace decision (docs/plans/phase3-launch-readiness-2026-09-21.md,
// BLOCKER 4) is NOT baked into the build. The default is the namespace
// Sonatype auto-verifies for a GitHub signup — `io.github.<username>` — which
// is the zero-paperwork option if no domain is ever registered.
group = providers.gradleProperty("nostosGroupId").getOrElse("io.github.unfazed-dev")
version = providers.gradleProperty("nostosVersion").getOrElse("0.2.0")

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

    // Central rejects a component without sources + javadoc jars
    // (central.sonatype.org/publish/requirements). AGP builds both.
    publishing {
        singleVariant("release") {
            withSourcesJar()
            withJavadocJar()
        }
    }
}

// -----------------------------------------------------------------------------
// Maven Central publishing (BLOCKER 3, docs/plans/phase3-launch-readiness-*.md)
// -----------------------------------------------------------------------------
// There is no OFFICIAL Gradle plugin for the Central Publishing Portal
// (central.sonatype.org/publish/publish-portal-gradle — checked 2026-09-21);
// every option there is a community plugin. So this stays on stock
// `maven-publish` + `signing`: publish into a local repo laid out as Maven
// expects, zip it, and the operator uploads that one bundle. No third-party
// plugin in the build, and nothing here assumes which namespace wins.
//
//   ./gradlew centralBundle -PnostosGroupId=io.github.you \
//       -PsigningInMemoryKey="$(gpg --armor --export-secret-keys KEYID)" \
//       -PsigningInMemoryKeyPassword=…
//   → android/build/distributions/nostos-kotlin-<version>-central-bundle.zip
//
// then POST it to https://central.sonatype.com/api/v1/publisher/upload with a
// Portal bearer token. That upload is the operator's call: it needs the
// account, the verified namespace and the GPG key, none of which live here.
publishing {
    publications {
        register<MavenPublication>("release") {
            artifactId = "nostos-kotlin"
            // AGP's `release` component only exists after evaluation.
            afterEvaluate { from(components["release"]) }
            pom {
                name.set("nostos-kotlin")
                description.set(
                    "Kotlin/Android SDK for Nostos — local-first sync over a Rust core (UniFFI).",
                )
                url.set("https://github.com/unfazed-dev/nostos")
                licenses {
                    license {
                        name.set("Apache License, Version 2.0")
                        url.set("https://www.apache.org/licenses/LICENSE-2.0.txt")
                    }
                }
                developers {
                    developer {
                        id.set("unfazed-dev")
                        name.set("Nostos maintainers")
                        url.set("https://github.com/unfazed-dev")
                    }
                }
                scm {
                    url.set("https://github.com/unfazed-dev/nostos")
                    connection.set("scm:git:https://github.com/unfazed-dev/nostos.git")
                    developerConnection.set("scm:git:ssh://git@github.com/unfazed-dev/nostos.git")
                }
            }
        }
    }
    repositories {
        // Staging only — never a live remote. Gradle writes the .md5/.sha1
        // Central requires alongside each artifact.
        maven {
            name = "centralBundle"
            url = uri(layout.buildDirectory.dir("central-bundle"))
        }
    }
}

signing {
    // Off unless a key is actually supplied, so `assembleRelease` and CI stay
    // green on a machine with no GPG at all.
    setRequired({ project.hasProperty("signingInMemoryKey") })
    if (project.hasProperty("signingInMemoryKey")) {
        useInMemoryPgpKeys(
            providers.gradleProperty("signingInMemoryKey").get(),
            providers.gradleProperty("signingInMemoryKeyPassword").getOrElse(""),
        )
    }
    sign(publishing.publications)
}

// The uploadable artifact. `maven-metadata*` is excluded: the Portal derives
// its own and rejects bundles that carry one.
tasks.register<Zip>("centralBundle") {
    group = "publishing"
    description = "Build the Central Portal upload bundle (zip of the staged Maven layout)."
    dependsOn("publishReleasePublicationToCentralBundleRepository")
    from(layout.buildDirectory.dir("central-bundle"))
    exclude("**/maven-metadata*")
    archiveFileName.set("nostos-kotlin-$version-central-bundle.zip")
    destinationDirectory.set(layout.buildDirectory.dir("distributions"))
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
