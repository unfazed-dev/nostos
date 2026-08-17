// Stress scenarios for the reactive facade (ADR-0024) — pure-Dart, no native
// library. Fires N=10_000 rapid `Collection<T>.upsert`s and `watch()` emissions
// against a fake `NostosEngine` (same seam as nostos_facade_test.dart) and asserts
// no drop/panic with the final count matching. Also runs the upsert flood
// repeatedly to surface flakiness.
//
// What this stresses:
//  - WRITE path: Collection.upsert -> toRow -> NostosDatabase.write -> engine.write
//    (drops are unambiguous: engine.writes.length MUST equal N exactly).
//  - READ path:  Collection.watch -> watchQuery -> engine.query (re-query per
//    change-tick) + jsonDecode + fromRow. The tick payload is ignored by
//    watchQuery (it re-runs SQL), so we assert no panic + final decode matches
//    the canned queryResult, not an exact emission count (broadcast/async
//    scheduling may coalesce).

import 'dart:async';

import 'package:nostos_flutter/src/nostos.dart';
import 'package:nostos_flutter/src/nostos_database.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

class _FakeEngine implements NostosEngine {
  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();
  final rowsController = StreamController<String>.broadcast();
  final stateController = StreamController<NostosConnectionState>.broadcast();
  String queryResult = '[]';
  final List<({String table, String op, String pk, String? payloadJson})>
  writes = [];

  @override
  Future<String> query({required String sql}) async => queryResult;

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) => stateController.stream;

  @override

  @override
  Future<String> subscribeStream({
    required String name,
    required String paramsJson,
  }) async => 'fake-stream';

  @override
  Future<void> unsubscribeStream({required String id}) async {}

  @override
  Stream<String> watch({required String table}) => rowsController.stream;

  final _writeStatus =
      StreamController<
        ({int pending, int deadLettered, String? lastError})
      >.broadcast();
  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => _writeStatus.stream;

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    writes.add((table: table, op: op, pk: pk, payloadJson: payloadJson));
    return 1;
  }

  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async => List.filled(ops.length, 1);

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) async => 1;

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) async => 1;

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) async => 0;

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) async => 0;

  @override
  void applySchema(List<ClientTableFfi> tables) {}

  @override
  Future<void> close() async {
    await rowsController.close();
    await stateController.close();
    await _writeStatus.close();
  }

  @override
  Future<void> signOut() async {}

  @override
  Future<void> setToken(String? token) async {}

  @override
  Future<void> disconnect() async {}

  @override
  Stream<NostosConnectionState> resume() => stateController.stream;
}

class _Todo {
  const _Todo(this.id, this.title);
  final String id;
  final String title;
  static _Todo fromRow(Map<String, dynamic> row) =>
      _Todo(row['id'] as String, row['title'] as String);
  Map<String, dynamic> toRow() => {'id': id, 'title': title};
}

void main() {
  const n = 10000;

  (_FakeEngine, NostosDatabase) newDb({String queryResult = '[]'}) {
    final engine = _FakeEngine()..queryResult = queryResult;
    final db = NostosDatabase.forTest(
      Nostos.withEngine(engine),
      const NostosSchema(tables: []),
    );
    return (engine, db);
  }

  test('stress WRITE: $n rapid upserts — no drop, count matches', () async {
    final (engine, db) = newDb();
    await db.subscribe('todos');
    final todos = db.collection<_Todo>(
      table: 'todos',
      fromRow: _Todo.fromRow,
      toRow: (t) => t.toRow(),
    );

    final sw = Stopwatch()..start();
    for (var i = 0; i < n; i++) {
      await todos.upsert(_Todo('id-$i', 't-$i'));
    }
    sw.stop();

    expect(engine.writes.length, n, reason: 'every upsert must be recorded');
    expect(engine.writes.last.pk, 'id-${n - 1}');
    expect(engine.writes.last.op, 'upsert');
    // Throughput log (assertion is correctness, not speed).
    final opsPerSec = (n / (sw.elapsedMilliseconds / 1000)).round();
    // ignore: avoid_print
    print(
      'STRESS write: $n upserts in ${sw.elapsedMilliseconds}ms '
      '($opsPerSec ops/s)',
    );
  });

  test('stress READ: $n watch ticks — no panic, final decode matches', () async {
    // watchQuery re-runs engine.query per tick; the canned result is what gets
    // decoded, so set it to a known single-row payload.
    const canned = '[{"id":"final","title":"done"}]';
    final (engine, db) = newDb(queryResult: canned);
    await db.subscribe('todos');
    final todos = db.collection<_Todo>(
      table: 'todos',
      fromRow: _Todo.fromRow,
      toRow: (t) => t.toRow(),
    );

    final emissions = <List<_Todo>>[];
    final errors = <Object>[];
    final sub = todos.watch().listen(emissions.add, onError: errors.add);

    final sw = Stopwatch()..start();
    for (var i = 0; i < n; i++) {
      // Payload is ignored by watchQuery (it re-queries); the tick just nudges.
      engine.rowsController.add('[]');
    }
    sw.stop();

    // Drain to completion: close the upstream and await the listener's done.
    await engine.rowsController.close();
    await sub.asFuture<void>();
    await sub.cancel();

    expect(errors, isEmpty, reason: 'watch stream must not error under load');
    expect(
      emissions,
      isNotEmpty,
      reason: 'watch must have emitted at least once',
    );
    expect(emissions.last.length, 1);
    expect(emissions.last.first.id, 'final');
    expect(emissions.last.first.title, 'done');
    final ticksPerSec = (n / (sw.elapsedMilliseconds / 1000)).round();
    // ignore: avoid_print
    print(
      'STRESS read: $n ticks (pump) in ${sw.elapsedMilliseconds}ms '
      '($ticksPerSec ticks/s, ${emissions.length} emissions decoded)',
    );
  });

  test('stress MIXED: concurrent upserts + watch ticks — no panic', () async {
    const canned = '[{"id":"final","title":"done"}]';
    final (engine, db) = newDb(queryResult: canned);
    await db.subscribe('todos');
    final todos = db.collection<_Todo>(
      table: 'todos',
      fromRow: _Todo.fromRow,
      toRow: (t) => t.toRow(),
    );

    final errors = <Object>[];
    final sub = todos.watch().listen((_) {}, onError: errors.add);

    final sw = Stopwatch()..start();
    // Interleave: upsert then tick, N times, without awaiting each upsert
    // individually (fire-and-forget into the engine) to maximise concurrency
    // pressure on the facade's state.
    final pending = <Future<int>>[];
    for (var i = 0; i < n; i++) {
      pending.add(todos.upsert(_Todo('id-$i', 't-$i')));
      engine.rowsController.add('[]');
    }
    await Future.wait(pending);
    sw.stop();
    await engine.rowsController.close();
    await sub.asFuture<void>();
    await sub.cancel();

    expect(errors, isEmpty);
    expect(
      engine.writes.length,
      n,
      reason: 'no upsert dropped under concurrency',
    );
    final mixedPerSec = (n / (sw.elapsedMilliseconds / 1000)).round();
    // ignore: avoid_print
    print(
      'STRESS mixed: $n upserts + $n ticks in ${sw.elapsedMilliseconds}ms '
      '($mixedPerSec cycles/s)',
    );
  });

  // Flakiness sweep: run the write flood 5x, assert deterministic count every
  // time and report variance in throughput.
  test('stress FLAKINESS: write flood x5, deterministic count', () async {
    final opsPerSec = <int>[];
    for (var run = 0; run < 5; run++) {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
        toRow: (t) => t.toRow(),
      );
      final sw = Stopwatch()..start();
      for (var i = 0; i < n; i++) {
        await todos.upsert(_Todo('id-$i', 't-$i'));
      }
      sw.stop();
      expect(engine.writes.length, n, reason: 'run $run dropped an upsert');
      opsPerSec.add((n / (sw.elapsedMilliseconds / 1000)).round());
    }
    final min = opsPerSec.reduce((a, b) => a < b ? a : b);
    final max = opsPerSec.reduce((a, b) => a > b ? a : b);
    // ignore: avoid_print
    print('STRESS flakiness x5 ops/s: $opsPerSec (min=$min max=$max)');
    // Throughput variance is fine; a >10x swing would hint at scheduler
    // pathology. We assert only correctness here.
    expect(
      max < min * 10 || min == 0,
      isTrue,
      reason: 'throughput swung >10x ($min..$max) — investigate',
    );
  });
}
