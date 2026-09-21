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
/// | writeBatch          | writeBatch      | `NostosSocket.writeBatch` (atomic)    |
/// | orSetAdd            | orSetAdd        | `NostosSocket.orSetAdd`               |
/// | orSetRemove         | orSetRemove     | `NostosSocket.orSetRemove`            |
/// | counterInc/Dec      | counterInc/Dec  | `NostosSocket.counterIncrement`/Dec   |
///
/// ## CRDT + atomic writeBatch (closed — Wave 4c, ADR-0036)
///
/// The wasm surface splits transport (`NostosSocket` — connected, ships) from
/// the full typed-verb engine (`NostosEngine` — in-process, no transport). Wave
/// 4c closed the gap by adding thin delegates on `NostosSocket` that reuse
/// `NostosEngine`'s CRDT logic (HLC mint via `nostos-domain`, `enqueue_batch`
/// atomicity) plus the ship-if-open step, mirroring `write`. No CRDT algebra is
/// re-implemented in the wasm crate; `NostosEngine`/native are untouched.
library;

import 'dart:async';

import 'engine.dart';
import 'worker_port.dart';

export 'worker_port.dart' show NostosWorkerPort, FakeNostosWorkerPort;

/// The storage backend the Worker reported active (ADR-0033). Surfaced on
/// `SyncStatus` so the UI can show "degraded" when OPFS is unavailable
/// (Safari Private Browsing) and the Worker fell back to memory, or when
/// another tab of the same origin already owns the OPFS store and this tab
/// opted out of proxying to it ([secondaryTab]; the default follower path
/// reports the leader's mode with reason "follower" instead).
enum NostosWebStorageMode { durable, memory, secondaryTab, unknown }

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
  Stream<NostosWebStorageMode> get storageModeStream =>
      _storageController.stream;

  /// Whether the browser granted this origin persistent (non-evictable)
  /// storage, as last reported by the Worker. `null` until the storage push
  /// arrives or when the browser has no StorageManager.
  bool? get storagePersisted => _storagePersisted;
  bool? _storagePersisted;

  @override
  Stream<bool> get webStorageDegraded =>
      storageModeStream.map((m) => m != NostosWebStorageMode.durable);

  // --------------------------------------------------------------------------
  // NostosEngine contract
  // --------------------------------------------------------------------------

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
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
        // CRDT-table tagging: the Worker calls NostosSocket.setCrdtTables with
        // these right after connect (ADR-0030 / ADR-0032 T4). Empty = no CRDT.
        'orSetTables': orSetTables.toList(),
        'counterTables': counterTables.toList(),
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
  Future<String> subscribeStream({
    required String name,
    required String paramsJson,
  }) => throw UnimplementedError(
    'sync streams are native-only in v1 (P5 §4: the web/WASM client has no '
    'mid-session subscribe channel yet)',
  );

  @override
  Future<void> unsubscribeStream({required String id}) => throw UnimplementedError(
    'sync streams are native-only in v1 (P5 §4: the web/WASM client has no '
    'mid-session subscribe channel yet)',
  );

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
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async {
    // Wave 4c (ADR-0036): atomic enqueue via the NostosSocket.writeBatch delegate
    // (one storage txn on the engine's enqueue_batch — a mid-batch failure
    // rolls back the whole batch). The Worker ships each op if OPEN; atomicity
    // is at the storage boundary, not the network send.
    final res = await _request({
      'cmd': 'writeBatch',
      'ops': ops
          .map(
            (o) => {
              'table': o.table,
              'op': o.op,
              'pk': o.pk,
              'payloadJson': o.payloadJson,
            },
          )
          .toList(),
    });
    final writeIds = res['writeIds'];
    if (writeIds is List) {
      return writeIds.map(_asInt).toList();
    }
    return const [];
  }

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) async {
    final res = await _request({
      'cmd': 'orSetAdd',
      'table': table,
      'pk': pk,
      'element': element,
    });
    return _asInt(res['writeId']);
  }

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) async {
    final res = await _request({
      'cmd': 'orSetRemove',
      'table': table,
      'pk': pk,
      'element': element,
    });
    return _asInt(res['writeId']);
  }

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) async {
    final res = await _request({
      'cmd': 'counterIncrement',
      'table': table,
      'pk': pk,
      'delta': delta,
    });
    return _asInt(res['writeId']);
  }

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) async {
    final res = await _request({
      'cmd': 'counterDecrement',
      'table': table,
      'pk': pk,
      'delta': delta,
    });
    return _asInt(res['writeId']);
  }

  @override
  Future<String> query({required String sql}) async {
    final res = await _request({'cmd': 'query', 'sql': sql});
    return (res['json'] as String? /* c8 ignore next 3 */ ) ?? '[]';
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
          final mode = switch (msg['mode']) {
            'durable' => NostosWebStorageMode.durable,
            _ when msg['reason'] == 'secondary-tab' =>
              NostosWebStorageMode.secondaryTab,
            _ => NostosWebStorageMode.memory,
          };
          _storageMode = mode;
          _storagePersisted = msg['persisted'] as bool?;
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
      await _request({
        'cmd': cmd,
      }).timeout(const Duration(seconds: 2), onTimeout: () => {});
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
