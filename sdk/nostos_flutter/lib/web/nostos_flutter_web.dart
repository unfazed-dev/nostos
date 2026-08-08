/// Flutter-web plugin registrant for `nostos_flutter` (ADR-0036).
///
/// Nostos-web uses **no platform channel**: the compile-time conditional import
/// (`engine_selector.dart`) selects [WebNostosEngine] over the shared
/// `nostos-ffi-wasm` Worker. This class exists solely so Flutter's plugin
/// tooling recognizes `web` as a supported platform (declared in
/// `pubspec.yaml`'s `flutter.plugin.platforms.web`) — without it, `flutter
/// build web` warns about a missing web implementation for the plugin. It
/// registers no method-channel handler.
library;

import 'package:flutter_web_plugins/flutter_web_plugins.dart';

/// No-op web registrant — see library doc.
class NostosFlutterWeb {
  static void registerWith(Registrar registrar) {
    // Intentionally empty: Nostos-web is Dart conditional-import based, not a
    // method-channel plugin. This exists only to satisfy platform discovery.
  }
}
