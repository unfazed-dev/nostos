/// `WebNostosEngine` — the Flutter-web implementation of the [NostosEngine]
/// seam (ADR-0036). Drives the *shared* `nostos-ffi-wasm` typed surface through
/// a durable-storage Worker via [NostosWorkerPort], so Flutter-web inherits
/// Wave-2 opfs-sahpool durability (NOT the rejected `frb_generated.web.dart`
/// path, which would compile a rusqlite crate to wasm and strand the app on an
/// in-memory backend).
///
/// This file is deliberately **free of `dart:js_interop`**: the Worker protocol
/// logic (request/response correlation, multi-table stream fan-out,
/// connection-state synthesis, write-status polling) is pure Dart and runs in
/// the plain VM under `flutter test`. The real JS Worker adapter lives in
/// `web_worker_port.dart` (web-only); tests inject a [FakeNostosWorkerPort].
///
/// ## Method map (NostosEngine → wasm surface via the Worker)
///
/// | NostosEngine verb    | Worker cmd      | wasm `NostosSocket` method            |
/// |---------------------|-----------------|--------------------------------------|
/// | subscribe           | connect         | `NostosSocket.connect` (+ `subscribe`)|
/// | watch               | watch/unwatch   | `onChange` → `query` per tick        |
/// | watchWriteStatus    | (pushed)        | `pendingCount`/`deadLetteredCount`/`lastError` |
/// | write               | write           | `NostosSocket.write`                  |
/// | query               | query           | `NostosSocket.query`                  |
/// | applySchema         | applySchema     | `NostosSocket.applySchema`            |
/// | setToken            | setToken        | reconnect w/ new token               |
/// | disconnect          | disconnect      | `NostosSocket.close` (engine kept)    |
/// | resume              | resume          | `NostosSocket.resume` / reconnect     |
/// | close               | close           | terminate Worker                     |
/// | signOut             | signOut         | `clearLocalState` + close            |
/// | writeBatch          | write (looped)  | `NostosSocket.write` × N (see ponytail)|
/// | orSetAdd/Remove     | —               | **GAP** (see below)                  |
/// | counterInc/Dec      | —               | **GAP** (see below)                  |
///
/// ## Gaps (reported, not self-resolved — ADR-0036)
///
/// The wasm surface splits transport (`NostosSocket` — connected, ships) from
/// the full typed-verb engine (`NostosEngine` — in-process, no transport). The
/// OR-set / PN-counter verbs (`orSetAdd`/`orSetRemove`/`counterIncrement`/
/// `counterDecrement`) live ONLY on the in-process `NostosEngine`, which mints a
/// client HLC (`nostos-domain`) and is unreachable from a connected socket.
/// `NostosSocket` exposes no CRDT verb. Reaching them would require either a
/// small `nostos-ffi-wasm` addition (delegate the CRDT verbs on `NostosSocket`,
/// mirroring how `applySchema`/`query`/`setCrdtTables` already delegate) or
/// Dart-side HLC minting (rejected — ADR-0035 keeps CRDT invariants in
/// `nostos-domain`, not re-implemented). Until that lands these throw
/// [UnsupportedError] with a pointer to the gap. `writeBatch` is wired as a
/// best-effort loop of single writes with a documented atomicity ceiling.
library;

import 'dart:async';

import 'engine.dart';
import 'worker_port.dart';

export 'worker_port.dart' show NostosWorkerPort, FakeNostosWorkerPort;

/// The storage backend the Worker reported active (ADR-0033). Surfaced on
/// `SyncStatus` so the UI can show "degraded" when OPFS is unavailable
/// (Safari Private Browsing) and the Worker fell back to memory.
enum NostosWebStorageMode { durable, memory, unknown }

/// Flutter-web [NostosEngine] over a nostos Worker ([NostosWorkerPort]).
class WebNostosEngine implements NostosEngine {
  WebNostosEngine._(this._port, {required this.url, this.token})
    : _workerOwned = true;

  /// Test/advanced constructor: drive an externally-owned port (the caller
  /// manages the Worker lifecycle). Used by the VM unit test with a
  /// [FakeNostosWorkerPort].
  WebNostosEngine.forPort(this._port, {required this.url, this.token})
    : _workerOwned = false;

  /// Spawn a [WebNostosEngine] connected to [url]. The Worker is created by the
  /// web-only adapter (`web_worker_port.dart`'s `spawnNostosWorker`); this pure-
  /// Dart constructor takes the already-open port.
  factory WebNostosEngine.connect({
    required String url,
    String? token,
    required NostosWorkerPort port,
  }) {
    final e = WebNostosEngine._(port, url: url, token: token);
    return e;
  }

  final NostosWorkerPort _port;
  final bool _workerOwned;

  /// The `/sync` URL (baked into the WS handshake; a token refresh reconnects).
  final String url;
  String? token;

  int _nextId = 1;
  bool _closed = false;
  NostosWebStorageMode _storageMode = NostosWebStorageMode.unknown;

  /// Pending request callbacks keyed by `id`. Each completes on the matching
  /// `{id, ok|error}` Worker response.
  final Map<int, _Pending> _pending = {};

  // ---- Reactive push sinks ----
  final StreamController<NostosConnectionState> _stateController =
      StreamController<NostosConnectionState>.broadcast();
  final Map<String, StreamController<String>> _watchControllers = {};
  final StreamController<({int pending, int deadLettered, String? lastError})>
  _writeStatusController =
      StreamController<
        ({int pending, int deadLettered, String? lastError})
      >.broadcast();
  final StreamController<NostosWebStorageMode> _storageController =
      StreamController<NostosWebStorageMode>.broadcast();

  StreamSubscription<Map<String, Object?>>? _sub;

  /// Start dispatching inbound Worker messages. Called once after the port is
  /// wired (the web adapter calls it; tests call it explicitly).
  void start() {
    _sub = _port.messages.listen(_onMessage);
  }

  /// The storage backend the Worker last reported. `unknown` until the Worker
  /// boots + inits sqlite-wasm (or degrades to memory). For `SyncStatus`.
  NostosWebStorageMode get storageMode => _storageMode;

  /// A stream of storage-mode transitions (durable/memory). For `SyncStatus`
  /// degrade surfacing. The Worker pushes the mode on boot (after sqlite-wasm
  /// init or degrade), so a listener bound before boot receives it; a late
  /// listener reads the cached [storageMode] getter.
  Stream<NostosWebStorageMode> get storageModeStream => _storageController.stream;

  @override
  Stream<bool> get webStorageDegraded =>
      storageModeStream.map((m) => m == NostosWebStorageMode.memory);

  // --------------------------------------------------------------------------
  // NostosEngine contract
  // --------------------------------------------------------------------------

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
  }) {
    // Emit `connecting` on the next microtask so a listener attached in the
    // same synchronous turn (the common `subscribe(...).listen(...)` pattern)
    // receives it — a broadcast controller drops events with no listener.
    scheduleMicrotask(
      () => _stateController.add(NostosConnectionState.connecting),
    );
    unawaited(
      _request({
        'cmd': 'connect',
        'url': url,
        'token': token,
        'tables': tables
            .map((t) => {'name': t.name, 'whereSql': t.whereSql})
            .toList(),
      }).then((_) {}).catchError((Object e) {
        _stateController.add(NostosConnectionState.disconnected);
      }),
    );
    return _stateController.stream;
  }

  @override
  Stream<String> watch({required String table}) {
    return (_watchControllers[table] ??= () {
      final c = StreamController<String>.broadcast();
      _watchControllers[table] = c;
      // Ask the Worker to push snapshots for this table (onChange ticks).
      _port.send({'cmd': 'watch', 'table': table});
      c.onCancel = () {
        _port.send({'cmd': 'unwatch'});
      };
      return c;
    }()).stream;
  }

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => _writeStatusController.stream;

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    final res = await _request({
      'cmd': 'write',
      'table': table,
      'op': op,
      'pk': pk,
      'payloadJson': payloadJson,
    });
    return _asInt(res['writeId']);
  }

  @override
  Future<List<int>> writeBatch({
    required List<
      ({String table, String op, String pk, String? payloadJson})
    > ops,
  }) async {
    // ponytail: NostosSocket exposes no batch verb, so this loops single
    // writes. That is NOT atomic — a disconnect mid-loop leaves a prefix
    // enqueued (each op is its own storage txn). The contract demands atomic
    // enqueue (ADR-0032 T3); the ceiling is reached when NostosSocket gains a
    // `writeBatch` delegate (small nostos-ffi-wasm addition mirroring how
    // applySchema/query already delegate to the socket's engine). Until then
    // this is best-effort: all ops apply_local + enqueue, shipping on the next
    // (re)connect flush. Reported as a gap in ADR-0036.
    final ids = <int>[];
    for (final o in ops) {
      ids.add(await write(table: o.table, op: o.op, pk: o.pk, payloadJson: o.payloadJson));
    }
    return ids;
  }

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) =>
      throw _crdtUnsupported('orSetAdd');

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) =>
      throw _crdtUnsupported('orSetRemove');

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) =>
      throw _crdtUnsupported('counterIncrement');

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) =>
      throw _crdtUnsupported('counterDecrement');

  @override
  Future<String> query({required String sql}) async {
    final res = await _request({'cmd': 'query', 'sql': sql});
    return (res['json'] as String? /* c8 ignore next 3 */) ?? '[]';
  }

  @override
  void applySchema(List<ClientTableFfi> tables) {
    // The contract is synchronous (the FFI throws on error). The Worker round-
    // trip is async, so we fire-and-await: an applySchema failure surfaces on
    // the connection-state stream as a disconnected transition + the error on
    // the write-status `lastError`. ponytail: a synchronous throw would require
    // a blocking Worker call (impossible on the main thread); the ceiling is a
    // pre-open schema cache applied inside the Worker on connect.
    unawaited(
      _request({
        'cmd': 'applySchema',
        'tables': tables
            .map((t) => {'name': t.name, 'columns': t.columns})
            .toList(),
      }).then((_) {}).catchError((Object e) {
        _stateController.add(NostosConnectionState.disconnected);
      }),
    );
  }

  @override
  Future<void> setToken(String? token) async {
    this.token = token;
    await _request({'cmd': 'setToken', 'token': token});
  }

  @override
  Future<void> disconnect() => _requestVoid({'cmd': 'disconnect'});

  @override
  Stream<NostosConnectionState> resume() {
    _stateController.add(NostosConnectionState.reconnecting);
    unawaited(
      _request({'cmd': 'resume'}).then((_) {}).catchError((Object e) {
        _stateController.add(NostosConnectionState.disconnected);
      }),
    );
    return _stateController.stream;
  }

  @override
  Future<void> close() async => _teardown(cmd: 'close');

  @override
  Future<void> signOut() async => _teardown(cmd: 'signOut');

  // --------------------------------------------------------------------------
  // Worker message dispatch
  // --------------------------------------------------------------------------

  void _onMessage(Map<String, Object?> msg) {
    // Unsolicited push (has `type`, no request `id`).
    final type = msg['type'];
    if (type != null) {
      switch (type) {
        case 'status':
          final connected = msg['connected'] == true;
          _stateController.add(
            connected
                ? NostosConnectionState.connected
                : NostosConnectionState.disconnected,
          );
        case 'snapshot':
          final table = msg['table'] as String?;
          final json = msg['json'] as String?;
          if (table != null && json != null) {
            _watchControllers[table]?.add(json);
          }
        case 'writeStatus':
          _writeStatusController.add((
            pending: _asInt(msg['pending']),
            deadLettered: _asInt(msg['deadLettered']),
            lastError: msg['lastError'] as String?,
          ));
        case 'storage':
          final mode = msg['mode'] == 'durable'
              ? NostosWebStorageMode.durable
              : NostosWebStorageMode.memory;
          _storageMode = mode;
          _storageController.add(mode);
      }
      return;
    }
    // Request response (has `id`).
    final id = _asInt(msg['id']);
    final p = _pending.remove(id);
    if (p == null) return;
    if (msg['error'] != null) {
      p.completeError(StateError('nostos worker: ${msg['error']}'));
    } else {
      p.complete(msg);
    }
  }

  Future<Map<String, Object?>> _request(Map<String, Object?> msg) {
    if (_closed) {
      return Future.error(StateError('WebNostosEngine is closed'));
    }
    final id = _nextId++;
    final p = _Pending();
    _pending[id] = p;
    _port.send({...msg, 'id': id});
    return p.future;
  }

  Future<void> _requestVoid(Map<String, Object?> msg) =>
      _request(msg).then((_) {});

  Future<void> _teardown({required String cmd}) async {
    if (_closed) return;
    _closed = true;
    try {
      await _request({'cmd': cmd}).timeout(
        const Duration(seconds: 2),
        onTimeout: () => {},
      );
    } catch (_) {
      // Best-effort: the Worker may already be gone.
    }
    await _sub?.cancel();
    await _stateController.close();
    await _writeStatusController.close();
    await _storageController.close();
    for (final c in _watchControllers.values) {
      await c.close();
    }
    _watchControllers.clear();
    if (_workerOwned) {
      _port.terminate();
    }
  }

  UnsupportedError _crdtUnsupported(String verb) => UnsupportedError(
    '$verb is unavailable on Flutter-web: the wasm NostosSocket (the connected, '
    'shipping surface) exposes no CRDT verb. The CRDT verbs live on the '
    'in-process NostosEngine, which mints a client HLC (nostos-domain) and has no '
    'transport. See ADR-0036 — reaching them needs a small nostos-ffi-wasm '
    'addition (delegate orSet/counter on NostosSocket).',
  );
}

/// Internal: a pending request's completer (typed as Map for all responses).
class _Pending {
  final completer = Completer<Map<String, Object?>>();
  Future<Map<String, Object?>> get future => completer.future;
  void complete(Map<String, Object?> v) => completer.complete(v);
  void completeError(Object e) => completer.completeError(e);
}

int _asInt(Object? v) {
  if (v is int) return v;
  if (v is num) return v.toInt();
  if (v is double) return v.toInt();
  return 0;
}
