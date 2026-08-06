// Beyond the brief's named test files (bench_math_test.dart, clock_test.dart)
// — added because Runner.writeAck/queueDrain subscribe to a broadcast
// `marks` stream that does not replay past events; a subscribe-after-trigger
// ordering bug would only surface as a 60s test hang, not a compile error,
// so it needs its own coverage. Flagged to team-lead in task-8-report.md.
import 'dart:async';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/bench/runner.dart';
import 'package:flutter_test/flutter_test.dart';

/// Minimal, self-contained fake adapter for Runner tests only — not shared
/// with Task 7's `test/adapter_conformance_test.dart` FakeAdapter, which is
/// private to that file and covers a different contract surface.
class _FakeAdapter implements SyncAdapter {
  _FakeAdapter(this.clock);

  final Stopwatch clock;
  final _sessions = <SessionRow>[];
  final _sessionsController = StreamController<List<SessionRow>>.broadcast();
  final _connectedController = StreamController<bool>.broadcast();
  final _marksController = StreamController<SyncMark>.broadcast();
  bool _connected = true;
  int _nextId = 0;
  Duration ackDelay = Duration.zero;

  @override
  String get engine => 'fake';

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {}

  @override
  Future<void> signOut() async {
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
    _sessionsController.add(List.unmodifiable(_sessions));
    _marksController.add(SyncMark(MarkKind.localVisible, id, clock.elapsed));
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
      _sessionsController.add(List.unmodifiable(_sessions));
      _marksController.add(SyncMark(MarkKind.serverAcked, id, clock.elapsed));
    });
  }

  @override
  Future<void> deleteSession(String id) async {
    _sessions.removeWhere((r) => r.id == id);
    _sessionsController.add(List.unmodifiable(_sessions));
  }

  @override
  Stream<List<SessionRow>> watchSessions() => _sessionsController.stream;

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

  /// Test-only: injects a remoteVisible mark directly, as if a row inserted
  /// by the harness (via PostgREST, outside SyncAdapter's surface) had just
  /// become visible. Used by the propagation() regression test below.
  void emitRemoteVisible(String rowId, DateTime serverCommittedAt) {
    _marksController.add(
      SyncMark(
        MarkKind.remoteVisible,
        rowId,
        clock.elapsed,
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

void main() {
  late Stopwatch clock;
  late _FakeAdapter adapter;
  late Runner runner;

  setUp(() {
    clock = Stopwatch()..start();
    adapter = _FakeAdapter(clock);
    runner = Runner(
      sdk: 'flutter',
      engine: 'fake',
      profile: 'local',
      specVersion: 'v0',
      seedSize: 25,
      appVersion: '1.0.0+1',
      device: {'model': 'test', 'os': 'test-os'},
      clock: clock,
    );
  });

  tearDown(() async {
    await adapter.dispose();
  });

  SessionRow buildSession(int i) => SessionRow(
    id: 'unused',
    title: 'Run $i',
    type: 'run',
    metric: 5000 + i,
    unit: 'm',
    occurredOn: DateTime.utc(2026, 1, 1),
  );

  test('writeAck completes with N samples under a real ack delay', () async {
    adapter.ackDelay = const Duration(milliseconds: 5);
    final record = await runner.writeAck(
      adapter,
      buildSession: buildSession,
      n: 5,
      timeout: const Duration(seconds: 2),
    );

    expect(record.runType, 'write_ack');
    expect(record.metrics['n'], 5);
    final samples = record.metrics['samples_ms'] as List;
    expect(samples, hasLength(5));
    for (final s in samples) {
      expect(s, greaterThanOrEqualTo(0));
    }
  });

  test('writeAck does not hang when acks are scheduled with zero delay', () async {
    adapter.ackDelay = Duration.zero;
    final record = await runner.writeAck(
      adapter,
      buildSession: buildSession,
      n: 3,
      timeout: const Duration(seconds: 2),
    );
    expect((record.metrics['samples_ms'] as List), hasLength(3));
  });

  test('queueDrain waits out an offline queue and measures from reconnect', () async {
    adapter.ackDelay = const Duration(milliseconds: 2);
    final record = await runner.queueDrain(
      adapter,
      buildSession: buildSession,
      n: 5,
      timeout: const Duration(seconds: 5),
    );

    expect(record.runType, 'queue_drain');
    expect(record.metrics['n'], 5);
    expect(record.metrics['queue_drain_ms'], greaterThanOrEqualTo(0));
  });

  test(
    'propagation adds clockOffset back (regression: was subtracting, '
    'giving true_delay - 2x offset)',
    () async {
      // Server clock reads 120ms ahead of the client, so a row committed
      // ~300ms ago (in true/server time) has a server_committed_at that is
      // only ~300ms - 120ms = 180ms behind DateTime.now() on the client's
      // own clock. Runner.propagation must add the 120ms back to recover
      // the true ~300ms delay, not subtract it (which would give ~60ms).
      const clockOffset = Duration(milliseconds: 120);
      const trueDelay = Duration(milliseconds: 300);
      final serverCommittedAt = DateTime.now().toUtc().subtract(
        trueDelay - clockOffset,
      );

      final record = await runner.propagation(
        adapter,
        insertRemoteRow: () async {
          final id = 'remote-row';
          // Emit asynchronously so propagation()'s buffered-mark path is
          // exercised the same way a real PostgREST round trip would be.
          scheduleMicrotask(
            () => adapter.emitRemoteVisible(id, serverCommittedAt),
          );
          return id;
        },
        clockOffset: clockOffset,
        n: 1,
        timeout: const Duration(seconds: 2),
      );

      final samples = record.metrics['samples_ms'] as List;
      expect(samples, hasLength(1));
      // Tolerance covers the real wall-clock time the test itself takes to
      // run between constructing serverCommittedAt and propagation()
      // reading DateTime.now() — typically sub-millisecond, never close to
      // the 240ms (2x offset) the sign bug would have produced.
      expect(samples.single, closeTo(300, 30));
    },
  );
}
