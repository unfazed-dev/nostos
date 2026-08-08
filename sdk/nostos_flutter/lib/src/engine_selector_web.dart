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
