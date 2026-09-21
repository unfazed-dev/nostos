// Task 14: bench runner wiring. Covers BenchHarness.runFullSuite (the
// per-adapter Core-4 + db_bytes orchestration) and
// runFullSuiteForBothEngines (the two-engine convenience wiring) entirely
// against FakeAdapter-style fixtures — no live Supabase/docker in this
// environment, see task-14-report.md for what wasn't run live.
import 'dart:async';
import 'dart:io';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/bench/harness.dart';
import 'package:atlet/bench/runner.dart';
import 'package:atlet/bench/store.dart';
import 'package:atlet/engine_registry.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fake_cart_orders.dart';

/// Self-contained fake, mirrors test/runner_test.dart's `_FakeAdapter` but
/// adds a settable [engine] name (so two-engine suite tests can distinguish
/// the two live adapters), a [seedSize]-driven cold-sync seed, and a
/// [signedOut] flag so runFullSuiteForBothEngines's post-suite signOut()
/// call is observable.
class _FakeAdapter with FakeCartOrdersDefaults implements SyncAdapter {
  _FakeAdapter({required this.engine, this.seedSize = 0});

  @override
  final String engine;
  final int seedSize;
  final _clock = Stopwatch()..start();
  final _sessions = <SessionRow>[];
  final _sessionsController = StreamController<List<SessionRow>>.broadcast();
  final _connectedController = StreamController<bool>.broadcast();
  final _marksController = StreamController<SyncMark>.broadcast();
  bool _connected = true;
  List<SessionRow>? _lastSessions;
  int _nextId = 0;

  void _emitSessions() {
    final rows = List<SessionRow>.unmodifiable(_sessions);
    _lastSessions = rows;
    _sessionsController.add(rows);
  }

  Duration ackDelay = Duration.zero;
  bool signedOut = false;
  int initCount = 0;
  String? lastDbDir;

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {
    initCount += 1;
    lastDbDir = dbDir;
    signedOut = false;
    for (var i = 0; i < seedSize; i++) {
      _sessions.add(
        SessionRow(
          id: 'seed-$i',
          title: 'Seed $i',
          type: 'run',
          metric: 1000,
          unit: 'm',
          occurredOn: DateTime.utc(2026, 1, 1),
          serverCommittedAt: DateTime.now().toUtc(),
        ),
      );
    }
    // Always emit, even when seedSize is 0: Runner.coldSync's listener
    // completes on the first emission where rows.length == seedSize, so a
    // seedSize-0 suite needs an (empty) emission too, not just a guard for
    // the non-empty case. watchSessions() replays this latest snapshot to
    // late subscribers, matching the real adapters' replayLatest contract.
    _emitSessions();
  }

  @override
  Future<void> signOut() async {
    signedOut = true;
    _sessions.clear();
  }

  @override
  Future<String> addSession(SessionRow s) async {
    final id = 'row-${_nextId++}';
    _sessions.add(
      SessionRow(
        id: id,
        title: s.title,
        type: s.type,
        metric: s.metric,
        unit: s.unit,
        note: s.note,
        streak: s.streak,
        occurredOn: s.occurredOn,
      ),
    );
    _emitSessions();
    _marksController.add(SyncMark(MarkKind.localVisible, id, _clock.elapsed));
    _scheduleAck(id);
    return id;
  }

  void _scheduleAck(String id) {
    if (!_connected) return;
    Future.delayed(ackDelay, () {
      if (!_connected) return;
      final index = _sessions.indexWhere((r) => r.id == id);
      if (index == -1) return;
      final row = _sessions[index];
      _sessions[index] = SessionRow(
        id: row.id,
        title: row.title,
        type: row.type,
        metric: row.metric,
        unit: row.unit,
        note: row.note,
        streak: row.streak,
        occurredOn: row.occurredOn,
        serverCommittedAt: DateTime.now().toUtc(),
      );
      _emitSessions();
      _marksController.add(SyncMark(MarkKind.serverAcked, id, _clock.elapsed));
    });
  }

  @override
  Future<void> deleteSession(String id) async {
    _sessions.removeWhere((r) => r.id == id);
    _emitSessions();
  }

  @override
  Stream<List<SessionRow>> watchSessions() =>
      replayLatest(_sessionsController.stream, () => _lastSessions);

  @override
  Stream<List<ProductRow>> watchProducts() => const Stream.empty();

  @override
  Stream<bool> get connected => _connectedController.stream;

  @override
  Future<void> setConnected(bool up) async {
    _connected = up;
    _connectedController.add(up);
    if (up) {
      for (final row in _sessions) {
        if (row.serverCommittedAt == null) _scheduleAck(row.id);
      }
    }
  }

  @override
  Stream<SyncMark> get marks => _marksController.stream;

  /// Test-only: simulates a row that arrived via the harness's PostgREST
  /// insert (propagation run) becoming visible through the normal read
  /// path — mirrors runner_test.dart's `emitRemoteVisible`.
  void emitRemoteVisible(String rowId, DateTime serverCommittedAt) {
    _marksController.add(
      SyncMark(
        MarkKind.remoteVisible,
        rowId,
        _clock.elapsed,
        serverCommittedAt: serverCommittedAt,
      ),
    );
  }

  Future<void> dispose() async {
    await _sessionsController.close();
    await _connectedController.close();
    await _marksController.close();
  }
}

SessionRow _buildSession(int i) => SessionRow(
  id: 'unused',
  title: 'Run $i',
  type: 'run',
  metric: 5000 + i,
  unit: 'm',
  occurredOn: DateTime.utc(2026, 1, 1),
);

/// Fake `insertRemoteRow`: mimics a PostgREST insert by handing back a
/// fresh id and, asynchronously, driving [adapter]'s normal watch path with
/// a remoteVisible mark — exactly the observable effect a real
/// `supabase.from('sessions').insert(...)` would eventually have once the
/// row synced back down. Counts calls so tests can assert propagation drove
/// exactly `n` PostgREST inserts.
({Future<String> Function() insertRemoteRow, int Function() callCount})
_fakeInsertRemoteRow(_FakeAdapter adapter) =>
    _fakeInsertRemoteRowFanOut([adapter]);

/// Same as [_fakeInsertRemoteRow] but fans the remoteVisible mark out to
/// every adapter in [adapters] — needed when one `insertRemoteRow` closure
/// is shared across a multi-engine suite (the real PostgREST insert is
/// engine-agnostic too), since whichever engine's suite is currently
/// subscribed to `.marks` is the one that actually needs the mark; the
/// others either aren't listening yet or already unsubscribed, so emitting
/// to all of them is harmless.
({Future<String> Function() insertRemoteRow, int Function() callCount})
_fakeInsertRemoteRowFanOut(List<_FakeAdapter> adapters) {
  var calls = 0;
  var nextId = 0;
  Future<String> insert() async {
    calls += 1;
    final id = 'remote-row-${nextId++}';
    final serverCommittedAt = DateTime.now().toUtc().subtract(
      const Duration(milliseconds: 10),
    );
    scheduleMicrotask(() {
      for (final adapter in adapters) {
        adapter.emitRemoteVisible(id, serverCommittedAt);
      }
    });
    return id;
  }

  return (insertRemoteRow: insert, callCount: () => calls);
}

void main() {
  group('BenchHarness.runFullSuite', () {
    late Directory tempDir;
    late _FakeAdapter adapter;
    late BenchStore store;
    late Runner runner;

    setUp(() async {
      tempDir = await Directory.systemTemp.createTemp('atlet-harness-test');
      adapter = _FakeAdapter(engine: 'cairn', seedSize: 2);
      adapter.ackDelay = const Duration(milliseconds: 2);
      store = BenchStore(directory: tempDir, fileName: 'runs.jsonl');
      runner = Runner(
        sdk: 'flutter',
        engine: 'cairn',
        profile: 'local',
        specVersion: 'v0',
        seedSize: 2,
        appVersion: '1.0.0+1',
        device: {'model': 'test', 'os': 'test-os'},
      );
    });

    tearDown(() async {
      await adapter.dispose();
      if (await tempDir.exists()) await tempDir.delete(recursive: true);
    });

    test(
      'runs cold_sync, propagation, write_ack, queue_drain, db_bytes in order '
      'and appends each to the store as it completes',
      () async {
        final fake = _fakeInsertRemoteRow(adapter);
        final dbDir = '${tempDir.path}/db';
        await Directory(dbDir).create(recursive: true);
        // Sanity payload for db_bytes: confirms the harness passes `dbDir`
        // through to Runner.dbBytes rather than measuring some other path.
        await File('$dbDir/cairn.sqlite').writeAsBytes(List.filled(42, 0));

        final harness = BenchHarness(
          runner: runner,
          adapter: adapter,
          store: store,
          supabaseUrl: 'http://localhost:3000',
          accessToken: 'test-token',
          userId: 'test-user',
          dbDir: dbDir,
          insertRemoteRow: fake.insertRemoteRow,
          buildSession: _buildSession,
          n: 2,
          timeout: const Duration(seconds: 5),
        );

        final records = await harness.runFullSuite();

        expect(records.map((r) => r.runType).toList(), [
          'cold_sync',
          'propagation',
          'write_ack',
          'queue_drain',
          'db_bytes',
        ]);
        expect(fake.callCount(), 2); // n=2 PostgREST inserts, one per sample

        final persisted = await store.readAll();
        expect(persisted, hasLength(5));
        expect(persisted.map((r) => r.runType).toList(), [
          'cold_sync',
          'propagation',
          'write_ack',
          'queue_drain',
          'db_bytes',
        ]);
        expect(persisted.every((r) => r.engine == 'cairn'), isTrue);

        final dbBytesRecord = persisted.last;
        expect(dbBytesRecord.metrics['db_bytes'], 42);
      },
    );

    test(
      'propagation never drives writes through the adapter (PostgREST only)',
      () async {
        final fake = _fakeInsertRemoteRow(adapter);
        final dbDir = '${tempDir.path}/db2';
        await Directory(dbDir).create(recursive: true);

        // watchSessions() replays the latest snapshot on listen (matching the
        // real adapters' replayLatest contract) — subscribe now so the
        // post-suite assertion sees the latest emission either way.
        List<SessionRow> latestRows = const [];
        final sub = adapter.watchSessions().listen((rows) => latestRows = rows);

        final harness = BenchHarness(
          runner: runner,
          adapter: adapter,
          store: store,
          supabaseUrl: 'http://localhost:3000',
          accessToken: 'test-token',
          userId: 'test-user',
          dbDir: dbDir,
          insertRemoteRow: fake.insertRemoteRow,
          buildSession: _buildSession,
          n: 3,
          timeout: const Duration(seconds: 5),
        );

        await harness.runFullSuite();
        await sub.cancel();

        // Only writeAck (n) + queueDrain (n) go through adapter.addSession;
        // propagation's n inserts must NOT add to that count.
        // seedSize (2) + writeAck (3) + queueDrain (3) = 8; propagation's 3
        // PostgREST inserts never touch this adapter's session list at all.
        expect(latestRows, hasLength(8));
        expect(fake.callCount(), 3);
      },
    );
  });

  group('runFullSuiteForBothEngines', () {
    late Directory tempDir;
    late BenchStore store;
    late _FakeAdapter nostosAdapter;
    late _FakeAdapter powerSyncAdapter;

    setUp(() async {
      tempDir = await Directory.systemTemp.createTemp(
        'atlet-both-engines-test',
      );
      store = BenchStore(directory: tempDir, fileName: 'runs.jsonl');
      nostosAdapter = _FakeAdapter(engine: 'cairn')
        ..ackDelay = const Duration(milliseconds: 1);
      powerSyncAdapter = _FakeAdapter(engine: 'powersync')
        ..ackDelay = const Duration(milliseconds: 1);
    });

    tearDown(() async {
      await nostosAdapter.dispose();
      await powerSyncAdapter.dispose();
      if (await tempDir.exists()) await tempDir.delete(recursive: true);
    });

    test('runs one full suite per engine, wipes each via signOut, and uses '
        'separate dbDir subdirectories', () async {
      // Fanned out to both adapters: insertRemoteRow is shared across
      // both engines' suites (matches production — one PostgREST client,
      // used regardless of which engine is currently under test), so the
      // fake must satisfy whichever engine's propagation run is currently
      // subscribed to `.marks`.
      final fake = _fakeInsertRemoteRowFanOut([nostosAdapter, powerSyncAdapter]);

      final results = await runFullSuiteForBothEngines(
        sdk: 'flutter',
        specVersion: 'v0',
        seedSize: 0,
        appVersion: '1.0.0+1',
        device: {'model': 'test', 'os': 'test-os'},
        rootDbDir: tempDir.path,
        supabaseUrl: 'http://localhost:3000',
        accessToken: 'test-token',
        userId: 'test-user',
        store: store,
        insertRemoteRow: fake.insertRemoteRow,
        buildSession: _buildSession,
        adapterFactories: {
          Engine.cairn: () => nostosAdapter,
          Engine.powersync: () => powerSyncAdapter,
        },
        n: 1,
        timeout: const Duration(seconds: 5),
      );

      expect(results.keys.toSet(), {Engine.cairn, Engine.powersync});
      expect(results[Engine.cairn], hasLength(5));
      expect(results[Engine.powersync], hasLength(5));

      expect(nostosAdapter.signedOut, isTrue);
      expect(powerSyncAdapter.signedOut, isTrue);

      expect(nostosAdapter.lastDbDir, '${tempDir.path}/cairn');
      expect(powerSyncAdapter.lastDbDir, '${tempDir.path}/powersync');
      expect(await Directory('${tempDir.path}/cairn').exists(), isTrue);
      expect(await Directory('${tempDir.path}/powersync').exists(), isTrue);

      final persisted = await store.readAll();
      expect(persisted, hasLength(10));
      expect(persisted.where((r) => r.engine == 'cairn'), hasLength(5));
      expect(persisted.where((r) => r.engine == 'powersync'), hasLength(5));
    });
  });

  group('sessionInsertPayload', () {
    test('omits id and user_id, formats occurred_on as date-only', () {
      final row = SessionRow(
        id: 'should-not-appear',
        title: 'Evening Row',
        type: 'time',
        metric: 1800,
        unit: 'sec',
        streak: 4,
        occurredOn: DateTime.utc(2026, 3, 7, 15, 30),
      );

      final payload = sessionInsertPayload(row);

      expect(payload.containsKey('id'), isFalse);
      expect(payload.containsKey('user_id'), isFalse);
      expect(payload['title'], 'Evening Row');
      expect(payload['type'], 'time');
      expect(payload['metric'], 1800);
      expect(payload['unit'], 'sec');
      expect(payload['streak'], 4);
      expect(payload['occurred_on'], '2026-03-07');
      expect(payload.containsKey('note'), isFalse);
    });

    test('includes note when present', () {
      final row = SessionRow(
        id: 'x',
        title: 'Run',
        type: 'distance',
        metric: 5000,
        unit: 'km',
        note: 'felt good',
        occurredOn: DateTime.utc(2026, 1, 1),
      );

      final payload = sessionInsertPayload(row);

      expect(payload['note'], 'felt good');
    });
  });
}
