# atlet

A new Flutter project.

## Getting Started

First `pub get` on a fresh checkout (or after `flutter clean`) on macOS:

```sh
mkdir -p build/ios/SourcePackages build/macos/SourcePackages   # Flutter 3.47 SwiftPM rsync bug
fvm flutter pub get
```

Flutter's SwiftPM step rsyncs plugins that depend on other plugins
(firebase_messaging → firebase_core) into `build/<os>/SourcePackages` without
creating the parent dir, so `pub get` fails with `rsync error (code 11)` until
those dirs exist. CI does the same `mkdir -p` (.github/workflows/ci.yml).

This project is a starting point for a Flutter application.

A few resources to get you started if this is your first Flutter project:

- [Learn Flutter](https://docs.flutter.dev/get-started/learn-flutter)
- [Write your first Flutter app](https://docs.flutter.dev/get-started/codelab)
- [Flutter learning resources](https://docs.flutter.dev/reference/learning-resources)

For help getting started with Flutter development, view the
[online documentation](https://docs.flutter.dev/), which offers tutorials,
samples, guidance on mobile development, and a full API reference.
