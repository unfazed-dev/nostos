// View-name normalization for non-public PG schemas (ADR-0028 known edge,
// fixed 2026-08-17): the engine creates one SQLite view per synced table,
// and a schema-qualified `myschema.tasks` arrives as the view
// `myschema_tasks` (Rust `view_name()` collapses dots). The structured
// Collection query paths (watch / getAll / watchOne / count / exists) must
// emit SQL against the COLLAPSED view name — before this fix they emitted
// `FROM myschema.tasks`, which SQLite parses as schema `myschema` and
// fails with "no such table". Pure-Dart: a recording fake NostosEngine.
//
// Note: raw-SQL callers (NostosDatabase.watch/getAll) get NO normalization —
// they must write the collapsed view name themselves (documented on
// _viewName). Only the structured paths normalize.

import 'dart:async';

import 'package:nostos_flutter/src/nostos.dart';
import 'package:nostos_flutter/src/nostos_database.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

/// Minimal fake engine that records every SQL string it is asked to run.
class _RecordingEngine implements NostosEngine {
  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();
  final rowsController = StreamController<String>.broadcast();
  final stateController = StreamController<NostosConnectionState>.broadcast();
  final List<String> queries = [];

  @override
  Future<String> query({required String sql}) async {
    queries.add(sql);
    return '[]';
  }

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) =>
      stateController.stream;

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
      StreamController<({int pending, int deadLettered, String? lastError})>
          .broadcast();
  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => _writeStatus.stream;

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async =>
      1;

  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async =>
      List.filled(ops.length, 1);

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) async =>
      1;

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) async =>
      1;

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) async =>
      0;

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) async =>
      0;

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

class _Row {
  const _Row(this.id);
  final String id;
  static _Row fromRow(Map<String, dynamic> row) => _Row(row['id'] as String);
}

void main() {
  (_RecordingEngine, NostosDatabase) newDb() {
    final engine = _RecordingEngine();
    final db = NostosDatabase.forTest(
      Nostos.withEngine(engine),
      const NostosSchema(tables: []),
    );
    return (engine, db);
  }

  Collection<_Row> schemaQualified(NostosDatabase db) => db.collection<_Row>(
    table: 'myschema.tasks',
    fromRow: _Row.fromRow,
  );

  test('getAll emits SQL against the collapsed view name', () async {
    final (engine, db) = newDb();
    await db.subscribe('myschema.tasks');
    await schemaQualified(db).getAll();
    expect(engine.queries, isNotEmpty);
    expect(
      engine.queries.last,
      contains('FROM myschema_tasks'),
      reason: 'schema-qualified table collapses to the engine view name',
    );
    expect(engine.queries.last, isNot(contains('myschema.tasks')));
  });

  test('watch composes against the collapsed view name', () async {
    final (engine, db) = newDb();
    await db.subscribe('myschema.tasks');
    final sub = schemaQualified(db).watch().listen((_) {});
    // Let the merged trigger streams attach (broadcast drops pre-subscribe
    // events), then fire a change-tick: watchQuery re-runs the SQL per tick.
    await Future<void>.delayed(const Duration(milliseconds: 50));
    engine.rowsController.add('[]');
    await Future<void>.delayed(const Duration(milliseconds: 50));
    await sub.cancel();
    expect(engine.queries, isNotEmpty);
    expect(engine.queries.last, contains('FROM myschema_tasks'));
  });

  test('count and exists collapse too', () async {
    final (engine, db) = newDb();
    await db.subscribe('myschema.tasks');
    final c = schemaQualified(db).count().listen((_) {});
    final e = schemaQualified(db).exists().listen((_) {});
    await Future<void>.delayed(const Duration(milliseconds: 50));
    engine.rowsController.add('[]');
    await Future<void>.delayed(const Duration(milliseconds: 50));
    await c.cancel();
    await e.cancel();
    expect(engine.queries.length, greaterThanOrEqualTo(2));
    for (final sql in engine.queries) {
      expect(sql, contains('myschema_tasks'), reason: sql);
      expect(sql, isNot(contains('myschema.tasks')), reason: sql);
    }
  });

  test('bare public names pass through unchanged', () async {
    final (engine, db) = newDb();
    await db.subscribe('todos');
    await db
        .collection<_Row>(table: 'todos', fromRow: _Row.fromRow)
        .getAll();
    expect(engine.queries.last, contains('FROM todos'));
  });
}