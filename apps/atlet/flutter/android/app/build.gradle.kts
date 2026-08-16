plugins {
    id("com.android.application")
    // Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// PILOT (ADR-0037): google-services only when the operator dropped
// android/app/google-services.json (never committed — operator-owned).
// Without it Firebase.initializeApp() is never called (ATLET_PUSH_PILOT
// dart-define gates the Dart side), so a config-less build stays green.
// NB: projectDir-anchored — a bare File("...") resolves against the gradle
// daemon's CWD and silently skips the plugin.
if (File(projectDir, "google-services.json").exists()) {
    apply(plugin = "com.google.gms.google-services")
}

android {
    namespace = "internal.atlet.atlet"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        // TODO: Specify your own unique Application ID (https://developer.android.com/studio/build/application-id.html).
        applicationId = "internal.atlet.atlet"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        minSdk = flutter.minSdkVersion
        targetSdk = flutter.targetSdkVersion
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    buildTypes {
        release {
            // TODO: Add your own signing config for the release build.
            // Signing with the debug keys for now, so `flutter run --release` works.
            signingConfig = signingConfigs.getByName("debug")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

dependencies {
    // AtletMessagingService (ADR-0037 §2 action pushes) subclasses
    // FlutterFire's messaging service; the plugin ships firebase-messaging
    // as `implementation`, which keeps com.google.* off the app's compile
    // classpath. Same version the plugin resolves, via the BoM.
    implementation(platform("com.google.firebase:firebase-bom:33.1.2"))
    implementation("com.google.firebase:firebase-messaging")
}
