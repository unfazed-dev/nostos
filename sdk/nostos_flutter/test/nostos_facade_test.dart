// Tests for the reactive facade (ADR-0024) layered on NostosDatabase:
// `Collection<T>` (typed watch/count/upsert/delete) and `SyncStatus`.
//
// Pure-Dart: injects a fake `NostosEngine` (no native library) via the
// `Nostos.withEngine` + `NostosDatabase.forTest` seams that engine.dart /
// nostos_database.dart document. Same pattern as nostos_test.dart and
// nostos_ws6_test.dart — the _FakeEngine here is a trimmed copy of
// FakeNostosEngine in nostos_test.dart (kept local so this file stays
// self-contained).
//
// Out of scope: the watchQuery replay/debounce machinery (covered in
// nostos_test.dart), integration tests, the native Rust engine.

import 'dart:async';
import 'dart:convert';

import 'package:nostos_flutter/src/nostos.dart';
import 'package:nostos_flutter/src/nostos_database.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

/// Minimal fake of [NostosEngine] for facade tests: serves a canned JSON result
/// for `query`, a broadcast row-tick stream for `watch`, and a broadcast
/// connection-state stream for `subscribe`/`resume`. Captures SQL in [queries]
/// so Collection.watch/count tests can assert the composed SQL verbatim.
class _FakeEngine implements NostosEngine {
  final rowsController = StreamController<String>.broadcast();
  final stateController = StreamController<NostosConnectionState>.broadcast();

  /// The JSON-array string returned by `query`. Tests set this to control the
  /// decoded result that watch/count consume.
  String queryResult = '[]';

  /// Every SQL the engine was asked to run, in call order.
  final List<String> queries = [];

  @override
  Future<String> query({required String sql}) async {
    queries.add(sql);
    return queryResult;
  }

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
  }) =>
      stateController.stream;

  @override
  Stream<String> watch({required String table}) => rowsController.stream;

  /// Drives `SyncStatus`'s write fields. Broadcast + no initial value: the real
  /// engine replays the current value on listen, but a test that never pushes
  /// should see the defaults.
  final writeStatusController = StreamController<
      ({int pending, int deadLettered, String? lastError})>.broadcast();

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
      watchWriteStatus() => writeStatusController.stream;

  /// Every (table, op, pk, payloadJson) the engine was asked to write, in order.
  final List<({String table, String op, String pk, String? payloadJson})>
      writes = [];

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
  void applySchema(List<ClientTableFfi> tables) {}

  @override
  Future<void> close() async {
    await rowsController.close();
    await stateController.close();
  }

  @override
  Future<void> disconnect() async {}

  @override
  Stream<NostosConnectionState> resume() => stateController.stream;
}

/// Tiny typed record for `Collection<Todo>` tests.
class _Todo {
  const _Todo(this.id, this.title);

  final String id;
  final String title;

  static _Todo fromRow(Map<String, dynamic> row) => _Todo(
        row['id'] as String,
        row['title'] as String,
      );

  Map<String, dynamic> toRow() => {'id': id, 'title': title};

  @override
  bool operator ==(Object other) =>
      other is _Todo && id == other.id && title == other.title;

  @override
  int get hashCode => Object.hash(id, title);

  @override
  String toString() => '_Todo($id, $title)';
}

void main() {
  // Builds a fresh db wired to a fresh fake engine, subscribed to `todos`.
  (_FakeEngine, NostosDatabase) newDb({String queryResult = '[]'}) {
    final engine = _FakeEngine()..queryResult = queryResult;
    final db = NostosDatabase.forTest(
      Nostos.withEngine(engine),
      const NostosSchema(tables: []),
    );
    return (engine, db);
  }

  group('Collection.upsert', () {
    test(
        'throws StateError when the collection was built WITHOUT toRow '
        '(read-only collection)', () async {
      final (_, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
        // no toRow — read-only handle
      );

      expect(
        () => todos.upsert(const _Todo('1', 'ship')),
        throwsA(isA<StateError>()),
      );
    });

    test(
        'throws ArgumentError when toRow() omits the pkColumn '
        '(default pkColumn="id")', () async {
      final (_, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
        // Deliberately omits the `id` primary-key column.
        toRow: (t) => {'title': t.title},
      );

      expect(
        () => todos.upsert(const _Todo('1', 'ship')),
        throwsA(isA<ArgumentError>()),
      );
    });
  });

  group('Collection.count', () {
    test('extracts the int from a {"count": 5} row', () async {
      final (engine, db) = newDb(queryResult: jsonEncode([
            {'count': 5}
          ]));
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      final future = todos.count().first;
      engine.rowsController.add('[]'); // one change-tick → one re-query
      final n = await future;

      expect(n, 5);
      // Composed SQL is the verbatim count-with-alias shape nostos_database emits.
      expect(engine.queries, ['SELECT COUNT(*) AS count FROM todos']);
    });

    test('returns 0 when count is non-numeric ({"count": "oops"})', () async {
      final (engine, db) = newDb(queryResult: jsonEncode([
            {'count': 'oops'}
          ]));
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      final future = todos.count().first;
      engine.rowsController.add('[]');
      final n = await future;

      expect(n, 0);
    });

    test('returns 0 when the result set is empty', () async {
      final (engine, db) = newDb(queryResult: '[]');
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      final future = todos.count().first;
      engine.rowsController.add('[]');
      final n = await future;

      expect(n, 0);
    });
  });

  group('Collection.watch', () {
    test(
        'emits SELECT * FROM <table> and maps rows via fromRow '
        '(no where)', () async {
      final rows = [
        {'id': '1', 'title': 'ship'},
        {'id': '2', 'title': 'review'},
      ];
      final (engine, db) = newDb(queryResult: jsonEncode(rows));
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      final future = todos.watch().first;
      engine.rowsController.add('[]'); // one change-tick → one re-query
      final items = await future;

      expect(items, const [_Todo('1', 'ship'), _Todo('2', 'review')]);
      expect(engine.queries, ['SELECT * FROM todos']);
    });

    test('composes "... WHERE <where>" when where: is supplied', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      final future = todos.watch(where: 'completed = 0').first;
      engine.rowsController.add('[]');
      await future;

      expect(engine.queries, ['SELECT * FROM todos WHERE completed = 0']);
    });
  });

  group('SyncStatus', () {
    test(
        'disconnected with null lastSyncedAt initially; flips to connected '
        'with a non-null lastSyncedAt after the engine emits connected', () async {
      final (engine, db) = newDb();
      // Accessing currentStatus wires the internal status listener
      // (_ensureStatusWired) — must happen BEFORE the emit so the listener
      // captures the transition. Before any subscribe/emit, the honest P0
      // snapshot is: disconnected, no last-sync stamp.
      final initial = db.currentStatus;
      expect(initial.connected, isFalse, reason: 'default conn is disconnected');
      expect(initial.lastSyncedAt, isNull, reason: 'never connected yet');

      // subscribe wires engine.stateController → Nostos.connectionState, which
      // the status listener listens to.
      await db.subscribe('todos');

      // Drive the state transition. Both the status listener and our
      // connectionState watcher are subscribed to the same broadcast stream;
      // await the watcher to guarantee propagation before reading currentStatus.
      final connectedFuture =
          db.connectionState.firstWhere((s) => s == NostosConnectionState.connected);
      engine.stateController.add(NostosConnectionState.connected);
      await connectedFuture;

      final after = db.currentStatus;
      expect(after.connected, isTrue);
      expect(after.lastSyncedAt, isNotNull);
    });

    test('pending writes surface without being reported as an error', () async {
      final (engine, db) = newDb();
      expect(db.currentStatus.pendingWrites, 0);
      expect(db.currentStatus.hasWriteError, isFalse);
      await db.subscribe('todos');

      final seen = db.status;
      engine.writeStatusController
          .add((pending: 2, deadLettered: 0, lastError: null));
      await pumpEventQueue();

      expect(seen.value.pendingWrites, 2);
      expect(seen.value.hasPendingWrites, isTrue);
      expect(
        seen.value.hasWriteError,
        isFalse,
        reason: 'queued-but-unsent is the offline-first promise, not a failure',
      );
    });

    test('a dead-lettered write surfaces the server message', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final seen = db.status;

      engine.writeStatusController.add((
        pending: 0,
        deadLettered: 1,
        lastError: "table not writable: 'todos'",
      ));
      await pumpEventQueue();

      expect(seen.value.hasWriteError, isTrue);
      expect(seen.value.lastWriteError, "table not writable: 'todos'");
      expect(seen.value.deadLetteredWrites, 1);
    });

    test('a connection change preserves the write fields', () async {
      // The regression this guards: two independent streams feed one
      // ValueNotifier, so a naive listener that rebuilds SyncStatus from only
      // its own stream silently wipes the other's fields — a reconnect would
      // erase a real "your write was lost" error mid-display.
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final seen = db.status;

      engine.writeStatusController
          .add((pending: 3, deadLettered: 1, lastError: 'boom'));
      await pumpEventQueue();
      expect(seen.value.pendingWrites, 3);

      final connectedFuture = db.connectionState
          .firstWhere((s) => s == NostosConnectionState.connected);
      engine.stateController.add(NostosConnectionState.connected);
      await connectedFuture;
      await pumpEventQueue();

      expect(seen.value.connected, isTrue);
      expect(seen.value.pendingWrites, 3, reason: 'not reset by a conn change');
      expect(seen.value.deadLetteredWrites, 1);
      expect(seen.value.lastWriteError, 'boom');
      expect(seen.value.uploading, isTrue, reason: 'connected + 3 pending');
    });
  });

  group('Collection writes (Map surface: upsertRow / patch)', () {
    test('upsertRow enqueues op:upsert with pk from row[pkColumn]', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos =
          db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      await todos.upsertRow({'id': '7', 'title': 'ship', 'completed': 0});

      expect(engine.writes.single.table, 'todos');
      expect(engine.writes.single.op, 'upsert');
      expect(engine.writes.single.pk, '7');
      expect(jsonDecode(engine.writes.single.payloadJson!), {
        'id': '7',
        'title': 'ship',
        'completed': 0,
      });
    });

    test('upsertRow throws ArgumentError when row omits pkColumn', () async {
      final (_, db) = newDb();
      await db.subscribe('todos');
      final todos =
          db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      expect(
        () => todos.upsertRow({'title': 'no id'}),
        throwsA(isA<ArgumentError>()),
      );
    });

    test('patch enqueues op:patch with only the supplied columns', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos =
          db.collection<_Todo>(table: 'todos', fromRow: _Todo.fromRow);

      await todos.patch('7', {'completed': 1});

      expect(engine.writes.single.table, 'todos');
      expect(engine.writes.single.op, 'patch');
      expect(engine.writes.single.pk, '7');
      expect(jsonDecode(engine.writes.single.payloadJson!), {'completed': 1});
    });
  });
}
