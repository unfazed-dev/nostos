// WS6 typed-record mapper tests. Proves watchMapped / getAllMapped decode rows
// into typed records via a user fromRow against real-shaped JSON (title String,
// completed bool) — the meaningful typed cast the fake-replicator integration
// test CAN'T exercise, because the fake server's payload is opaque bytes and
// the json_extract'd columns come back NULL. There only `_pk` is populated; the
// meaningful field casts live here.
//
// Uses Nostos.withEngine + NostosDatabase.forTest + a FakeEngine (no native
// library) — the seams engine.dart / nostos_database.dart document for exactly
// this kind of pure-Dart test.
import 'dart:async';
import 'dart:convert';

import 'package:nostos_flutter/src/nostos.dart';
import 'package:nostos_flutter/src/nostos_database.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  // Real-shaped rows (what a Postgres-backed deployment's /sync delivers) —
  // the JSON token shape PgReplicator's append_typed_value emits.
  const rows = [
    {'_pk': '1', 'title': 'ship', 'completed': false},
    {'_pk': '2', 'title': 'review', 'completed': true},
  ];
  final queryResult = jsonEncode(rows);

  test('watchMapped decodes rows into typed records', () async {
    // `Stream.value('[]')` is one change-tick (content ignored by watchQuery —
    // it re-runs `SELECT` via query on every tick). Delivered asynchronously
    // when the watchQuery listener attaches to the broadcast _rowsStream, so no
    // pre-listener event is dropped.
    final nostos = Nostos.withEngine(
      _FakeEngine(queryResult: queryResult, rows: Stream.value('[]')),
    );
    await nostos.subscribe('tasks');

    final tasks = await nostos
        .watchMapped<Task>('SELECT * FROM tasks', Task.fromRow)
        .first
        .timeout(const Duration(seconds: 5));

    expect(tasks, [
      const Task(pk: '1', title: 'ship', completed: false),
      const Task(pk: '2', title: 'review', completed: true),
    ]);
  });

  test('getAllMapped decodes rows into typed records', () async {
    final db = NostosDatabase.forTest(
      Nostos.withEngine(
        _FakeEngine(
          queryResult: queryResult,
          rows: const Stream<String>.empty(),
        ),
      ),
      const NostosSchema(tables: []),
    );
    await db.subscribe('tasks');

    final tasks = await db.getAllMapped<Task>(
      'SELECT * FROM tasks',
      Task.fromRow,
    );

    expect(tasks, [
      const Task(pk: '1', title: 'ship', completed: false),
      const Task(pk: '2', title: 'review', completed: true),
    ]);
  });

  test('NostosSchema.fromSchemaDescriptor parses column affinity + pg_oid (WS6-A)', () {
    // Mirrors the wire shape PgSchemaSource emits (ports.rs SchemaColumn) —
    // affinity derived via oid_to_sqlite_affinity (ADR-0019): bool(16)→INTEGER,
    // int4(23)→INTEGER, float4(700)→REAL, text(25)→TEXT.
    final schema = NostosSchema.fromSchemaDescriptor({
      'publication': 'cairn_pub',
      'tables': [
        {
          'name': 'tasks',
          'primary_key': ['id'],
          'columns': [
            {'name': 'id', 'pg_oid': 25, 'affinity': 'TEXT'},
            {'name': 'title', 'pg_oid': 25, 'affinity': 'TEXT'},
            {'name': 'completed', 'pg_oid': 16, 'affinity': 'INTEGER'},
            {'name': 'position', 'pg_oid': 700, 'affinity': 'REAL'},
          ],
        },
      ],
    });
    final cols = schema.tables.single.columns;
    expect(cols.map((c) => c.name), ['id', 'title', 'completed', 'position']);
    expect(cols[0].affinity, 'TEXT');
    expect(cols[0].pgOid, 25);
    expect(cols[2].affinity, 'INTEGER');
    expect(cols[2].pgOid, 16);
    expect(cols[3].affinity, 'REAL');
    expect(cols[3].pgOid, 700);
  });
}

/// A minimal typed record decoded from a row, with a `fromRow` factory — the
/// PowerSync-parity convention this WS6 mapper wraps.
class Task {
  const Task({required this.pk, required this.title, required this.completed});

  final String pk;
  final String title;
  final bool completed;

  factory Task.fromRow(Map<String, dynamic> row) => Task(
        pk: row['_pk'] as String,
        title: row['title'] as String,
        completed: row['completed'] as bool,
      );

  @override
  bool operator ==(Object other) =>
      other is Task &&
      pk == other.pk &&
      title == other.title &&
      completed == other.completed;

  @override
  int get hashCode => Object.hash(pk, title, completed);

  @override
  String toString() => 'Task($pk, $title, $completed)';
}

/// A no-native-library [NostosEngine]: serves a canned JSON query result and a
/// canned change-tick stream. Implements the full interface so any [Nostos]
/// method can be exercised without the FFI.
class _FakeEngine implements NostosEngine {
  _FakeEngine({required this.queryResult, required this.rows});

  final String queryResult;
  final Stream<String> rows;

  @override
  Stream<NostosConnectionState> subscribe({required List<NostosTableSub> tables}) =>
      const Stream<NostosConnectionState>.empty();

  @override
  Stream<String> watch({required String table}) => rows;

  @override
  Future<String> query({required String sql}) async => queryResult;

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async => 0;

  @override
  void applySchema(List<ClientTableFfi> tables) {}

  @override
  Future<void> close() async {}

  @override
  Future<void> disconnect() async {}

  @override
  Stream<NostosConnectionState> resume() =>
      const Stream<NostosConnectionState>.empty();
}
