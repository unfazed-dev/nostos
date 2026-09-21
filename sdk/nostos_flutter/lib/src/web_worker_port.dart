/// The web-only `dart:js_interop` adapter that connects [WebNostosEngine]'s
/// pure-Dart [NostosWorkerPort] to a real browser Worker (ADR-0036).
///
/// This file is imported ONLY on web (via `engine_selector_web.dart`), so it
/// may freely use `dart:js_interop` / `package:web`. It spawns the nostos
/// Worker (`nostos_worker.js`, shipped as a web asset), which in turn loads the
/// shared `nostos-ffi-wasm` artifact + sqlite-wasm (opfs-sahpool, ADR-0033) and
/// owns the live `NostosSocket`. Dart ↔ Worker messages are plain JSON objects
/// (`jsify` outbound, `dartify` inbound).
library;

import 'dart:async';
import 'dart:js_interop';

import 'package:web/web.dart';

import 'worker_port.dart';

/// Default Worker script URL, relative to the document base. Apps override via
/// `Nostos.connect()` → `createNostosEngine(workerUrl:)` when their asset layout
/// differs. The Worker + wasm + sqlite-wasm must be served at this URL's
/// directory (see `web/nostos/` and the ADR-0036 bootstrap notes).
const defaultWorkerUrl = 'nostos/nostos_worker.js';

/// Spawn the nostos Worker at [workerUrl] and return a [NostosWorkerPort] backed
/// by the real browser Worker. The Worker boots the wasm engine + sqlite-wasm
/// (durable, or memory degrade) asynchronously; the engine learns the resolved
/// storage mode from the Worker's `{type:"storage", mode}` push.
NostosWorkerPort spawnNostosWorker({String? workerUrl}) {
  // MDN Storage API: origin storage is best-effort (evictable under pressure)
  // until `persist()` is granted. StorageManager.persist is Window-only, so it
  // runs here, not in the Worker; the Worker reports `persisted` back on its
  // storage push (WebNostosEngine.storagePersisted). Firefox prompts the user;
  // Chromium/Safari decide by heuristics — either way, fire and forget.
  try {
    unawaited(
      window.navigator.storage.persist().toDart.catchError((Object _) => false.toJS),
    );
  } catch (_) {
    /* no StorageManager (very old browser) — nothing to request */
  }
  return _JsWorkerPort(workerUrl ?? defaultWorkerUrl);
}

/// A [NostosWorkerPort] over a real browser [Worker].
class _JsWorkerPort implements NostosWorkerPort {
  // {type: 'module'} is REQUIRED: nostos_worker.js is an ES module (static
  // imports of the wasm artifact). A classic Worker — the default when
  // options are omitted — dies at parse, silently, and every request then
  // hangs forever (the plain-JS e2e harness always passed {type:'module'},
  // which is why only Flutter-web hit this).
  _JsWorkerPort(String url)
    : _worker = Worker(url.toJS, WorkerOptions(type: 'module'));

  final Worker _worker;
  final _controller = StreamController<Map<String, Object?>>.broadcast();

  bool _wired = false;

  void _wire() {
    if (_wired) return;
    _wired = true;
    // package:web exposes Worker.onmessage as a settable EventHandler
    // (JSFunction). Convert a Dart closure → JSFunction; the Worker invokes it
    // with a MessageEvent on every inbound postMessage.
    _worker.onmessage = ((MessageEvent e) {
      final raw = e.data?.dartify();
      // dartify() of a JS object yields Map<dynamic, dynamic> — the old
      // `is Map<String, Object?>` guard never matched, so EVERY worker→Dart
      // message (snapshots, status, request responses) was silently dropped.
      // Re-key instead. The VM tests miss this by construction: their fake
      // port feeds plain Dart maps, never a dartify() result.
      if (raw is Map) {
        _controller.add(Map<String, Object?>.from(raw));
      }
    }).toJS;
  }

  @override
  void send(Map<String, Object?> msg) {
    _wire();
    _worker.postMessage(msg.jsify());
  }

  @override
  Stream<Map<String, Object?>> get messages {
    _wire();
    return _controller.stream;
  }

  @override
  void terminate() {
    _worker.terminate();
    _controller.close();
  }
}
