/// Web [NostosEngine] factory — the `dart.library.js_interop` conditional-import
/// arm (ADR-0036). See `engine_selector.dart` for the selection rationale.
///
/// This file is compiled ONLY on web (the barrel's conditional import sees to
/// that), so it may freely use `dart:js_interop` / `package:web` without
/// affecting native builds. It never touches `RustLib.init` or
/// `RustNostosEngine` — keeping `flutter build web` off the rejected
/// `frb_generated.web.dart` path entirely.
library;

import 'dart:async';

import 'engine.dart';
import 'engine_web.dart';
import 'web_worker_port.dart';

/// Create the web [NostosEngine] ([WebNostosEngine]) over the shared
/// `nostos-ffi-wasm` backend, driven through a durable-storage Worker
/// (opfs-sahpool, ADR-0033). [sqlitePath] is ignored on web — durability is
/// OPFS-backed, not a filesystem path.
///
/// [workerUrl] overrides where the nostos Worker script is served from (default
/// `nostos/nostos_worker.js`). The Worker + wasm + sqlite-wasm assets must be
/// served at that URL's directory — see ADR-0036's bootstrap section.
Future<NostosEngine> createNostosEngine({
  required String url,
  String? token,
  String? sqlitePath,
  String? workerUrl,
}) async {
  final port = spawnNostosWorker(workerUrl: workerUrl);
  final engine = WebNostosEngine.connect(url: url, token: token, port: port)
    ..start();
  return engine;
}

/// Direct mode is native-only for now: the loop lives in `nostos-client`
/// (tokio + reqwest), and the web backend is `nostos-ffi-wasm`, which has
/// neither. Throwing here keeps `flutter build web` compiling for an app that
/// offers both modes, and fails loudly on the platform that cannot serve it.
///
/// ponytail: the ceiling is the transport, not the protocol — `PullCursor` is
/// already WASM-clean. Porting means a fetch/WebSocket source behind the same
/// `apply`/`apply_snapshot` calls in `nostos-ffi-wasm`.
Future<NostosEngine> createDirectNostosEngine({
  required String supabaseUrl,
  required String anonKey,
  required String scope,
  String? token,
  String? sqlitePath,
  Map<String, String> counterFields = const <String, String>{},
  bool keepLocalOnSignOut = false,
}) async {
  throw UnsupportedError(
    'direct mode is not available on web yet — use Nostos.connect against a '
    'nostos-server, or run this build on iOS/Android/desktop.',
  );
}
