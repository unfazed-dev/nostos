/// The web-only `dart:js_interop` adapter that connects [WebNostosEngine]'s
/// pure-Dart [NostosWorkerPort] to a browser SharedWorker (ADR-0036, ADR-0051).
///
/// This file is imported ONLY on web (via `engine_selector_web.dart`), so it
/// may freely use `dart:js_interop` / `package:web`. The SharedWorker broker
/// (`nostos_broker.js`) authenticates each tab's private port. Its host tab
/// starts `nostos_worker.js`, which loads `nostos-ffi-wasm` and sqlite-wasm
/// (opfs-sahpool, ADR-0033). Dart ↔ Worker messages are plain JSON objects.
library;

import 'dart:async';
import 'dart:js_interop';

import 'package:web/web.dart';

import 'worker_port.dart';

/// Default SharedWorker broker URL, relative to the document base. Apps override via
/// `Nostos.connect()` → `createNostosEngine(workerUrl:)` when their asset layout
/// differs. The broker, engine Worker, wasm, and sqlite-wasm must be served at this URL's
/// directory (see `web/nostos/` and the ADR-0036 bootstrap notes).
const defaultWorkerUrl = 'nostos/nostos_broker.js';

/// Connect to the broker at [workerUrl] and return its private browser port.
/// The authenticated host page starts a dedicated Worker for wasm + OPFS;
/// the engine learns its storage mode from a `{type:"storage", mode}` push.
NostosWorkerPort spawnNostosWorker({String? workerUrl}) {
  // MDN Storage API: origin storage is best-effort (evictable under pressure)
  // until `persist()` is granted. StorageManager.persist is Window-only, so it
  // runs here, not in the Worker; the Worker reports `persisted` back on its
  // storage push (WebNostosEngine.storagePersisted). Firefox prompts the user;
  // Chromium/Safari decide by heuristics — either way, fire and forget.
  try {
    unawaited(
      window.navigator.storage.persist().toDart.catchError(
        (Object _) => false.toJS,
      ),
    );
  } catch (_) {
    /* no StorageManager (very old browser) — nothing to request */
  }
  return _JsWorkerPort(workerUrl ?? defaultWorkerUrl);
}

/// A [NostosWorkerPort] over a private SharedWorker MessagePort. The broker
/// asks the authenticated host tab to create one dedicated OPFS engine Worker
/// and transfer its engine port back to the broker.
class _JsWorkerPort implements NostosWorkerPort {
  // {type: 'module'} is REQUIRED: the broker and engine are ES modules. A
  // classic Worker — the default when
  // options are omitted — dies at parse, silently, and every request then
  // hangs forever (the plain-JS e2e harness always passed {type:'module'},
  // which is why only Flutter-web hit this).
  _JsWorkerPort(String url)
    : _engineUrl = url.replaceFirst('nostos_broker.js', 'nostos_worker.js') {
    _pageHideListener = ((Event _) {
      // A closing host page must release its OPFS Web Lock so another tab can
      // take over. The broker receives only a close signal, never page data.
      _shared?.port.postMessage({'cmd': 'close'}.jsify());
      _engineWorker?.terminate();
    }).toJS;
    window.addEventListener('pagehide', _pageHideListener);
    try {
      _shared = SharedWorker(url.toJS, WorkerOptions(type: 'module'));
      _shared!.port.start();
    } catch (_) {
      // Older browsers retain a single-tab dedicated path. Its Web Lock
      // refuses a second tab instead of exposing rows through a public bus.
      _dedicated = Worker(_engineUrl.toJS, WorkerOptions(type: 'module'));
    }
  }

  final String _engineUrl;
  late final EventListener _pageHideListener;
  SharedWorker? _shared;
  Worker? _dedicated;
  Worker? _engineWorker;
  final _controller = StreamController<Map<String, Object?>>.broadcast();

  bool _wired = false;

  void _wire() {
    if (_wired) return;
    _wired = true;
    // package:web exposes Worker.onmessage as a settable EventHandler
    // (JSFunction). Convert a Dart closure → JSFunction; the Worker invokes it
    // with a MessageEvent on every inbound postMessage.
    final handler = ((MessageEvent e) {
      final raw = e.data?.dartify();
      // dartify() of a JS object yields Map<dynamic, dynamic> — the old
      // `is Map<String, Object?>` guard never matched, so EVERY worker→Dart
      // message (snapshots, status, request responses) was silently dropped.
      // Re-key instead. The VM tests miss this by construction: their fake
      // port feeds plain Dart maps, never a dartify() result.
      if (raw is Map) {
        if (raw['type'] == 'retireEngine') {
          _engineWorker?.terminate();
          _engineWorker = null;
          return;
        }
        if (raw['type'] == 'needEngine' && _shared != null) {
          _attachEngine();
          return;
        }
        _controller.add(Map<String, Object?>.from(raw));
      }
    }).toJS;
    if (_shared != null) {
      _shared!.port.onmessage = handler;
    } else {
      _dedicated!.onmessage = handler;
    }
  }

  void _attachEngine() {
    _engineWorker?.terminate();
    final worker = Worker(_engineUrl.toJS, WorkerOptions(type: 'module'));
    final channel = MessageChannel();
    worker.postMessage(
      {'cmd': 'attachPort', 'port': channel.port1}.jsify(),
      <JSAny?>[channel.port1].toJS,
    );
    _shared!.port.postMessage(
      {'cmd': 'attachEngine', 'port': channel.port2}.jsify(),
      <JSAny?>[channel.port2].toJS,
    );
    _engineWorker = worker;
  }

  @override
  void send(Map<String, Object?> msg) {
    _wire();
    if (_shared != null) {
      _shared!.port.postMessage(msg.jsify());
    } else {
      _dedicated!.postMessage(msg.jsify());
    }
  }

  @override
  Stream<Map<String, Object?>> get messages {
    _wire();
    return _controller.stream;
  }

  @override
  void terminate() {
    window.removeEventListener('pagehide', _pageHideListener);
    _shared?.port.postMessage({'cmd': 'close'}.jsify());
    _engineWorker?.terminate();
    _shared?.port.close();
    _dedicated?.terminate();
    _controller.close();
  }
}
