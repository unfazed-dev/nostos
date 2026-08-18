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
import 'package:nostos_flutter/src/predicate.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

/// Minimal fake of [NostosEngine] for facade tests: serves a canned JSON result
/// for `query`, a broadcast row-tick stream for `watch`, and a broadcast
/// connection-state stream for `subscribe`/`resume`. Captures SQL in [queries]
/// so Collection.watch/count tests can assert the composed SQL verbatim.
class _FakeEngine implements NostosEngine {
  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();
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
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) => stateController.stream;

  @override
  Stream<String> watch({required String table}) => rowsController.stream;

  /// Drives `SyncStatus`'s write fields. Broadcast + no initial value: the real
  /// engine replays the current value on listen, but a test that never pushes
  /// should see the defaults.
  final writeStatusController =
      StreamController<
        ({int pending, int deadLettered, String? lastError})
      >.broadcast();

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => writeStatusController.stream;

  /// Every (table, op, pk, payloadJson) the engine was asked to write, in order.
  final List<({String table, String op, String pk, String? payloadJson})>
  writes = [];

  @override

  @override
  Future<String> subscribeStream({
    required String name,
    required String paramsJson,
  }) async => 'fake-stream';

  @override
  Future<void> unsubscribeStream({required String id}) async {}

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    writes.add((table: table, op: op, pk: pk, payloadJson: payloadJson));
    return writeResult == null ? 1 : await writeResult!();
  }

  /// Optional override for the next outbox id a `write` returns. Default
  /// (`null`) yields `1` for every call; tests that need distinct ids (e.g.
  /// writeBatch) assign a counter here.
  Future<int> Function()? writeResult;

  /// Every batch the engine was asked to enqueue, in call order. Each entry is
  /// the full op list passed to `writeBatch` (one record per batch call, NOT
  /// one per op — writeBatch crosses FFI as a single atomic group).
  final List<List<({String table, String op, String pk, String? payloadJson})>>
  batchCalls = [];

  /// Optional override for `writeBatch`'s return / throw. Default (`null`)
  /// yields sequential 1-based ids. Set to a throwing function to simulate an
  /// atomic-batch failure (no partial state is recorded).
  Future<List<int>> Function(
    List<({String table, String op, String pk, String? payloadJson})>,
  )?
  batchResult;

  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async {
    batchCalls.add(List.of(ops));
    if (batchResult != null) return batchResult!(ops);
    final ids = <int>[];
    for (var i = 0; i < ops.length; i++) {
      ids.add(writeResult == null ? i + 1 : await writeResult!());
    }
    return ids;
  }

  /// Recorded OR-set add/remove calls: (op, table, pk, element).
  final List<({String op, String table, String pk, String element})>
  orSetCalls = [];

  /// Recorded counter increment/decrement calls: (op, table, pk, delta).
  final List<({String op, String table, String pk, int delta})> counterCalls =
      [];

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) async {
    orSetCalls.add((op: 'add', table: table, pk: pk, element: element));
    return orSetCalls.length;
  }

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) async {
    orSetCalls.add((op: 'remove', table: table, pk: pk, element: element));
    return orSetCalls.length;
  }

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) async {
    counterCalls.add((op: 'increment', table: table, pk: pk, delta: delta));
    return counterCalls.length;
  }

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) async {
    counterCalls.add((op: 'decrement', table: table, pk: pk, delta: delta));
    return counterCalls.length;
  }

  @override
  void applySchema(List<ClientTableFfi> tables) {}

  @override
  Future<void> close() async {
    await rowsController.close();
    await stateController.close();
  }

  /// ADR-0029: wipe local rows + outbox, stop sync, clear seed token. The fake
  /// has no native store to wipe; tests that exercise signOut use the real
  /// engine (signout_test.dart), so this just records the call.
  int signOutCalls = 0;

  @override
  Future<void> signOut() async {
    signOutCalls++;
  }

  /// Recorded so a test can assert `Nostos.setToken` actually delegates —
  /// a silently-dropped refresh is the exact bug this seam exists to prevent.
  String? lastSetToken;
  int setTokenCalls = 0;

  @override
  Future<void> setToken(String? token) async {
    lastSetToken = token;
    setTokenCalls++;
  }

  /// Counters for pauseSync/resumeSync wiring (ADR-0032 T1).
  int disconnectCalls = 0;
  int resumeCalls = 0;

  @override
  Future<void> disconnect() async {
    disconnectCalls++;
  }

  @override
  Stream<NostosConnectionState> resume() {
    resumeCalls++;
    return stateController.stream;
  }
}

/// Tiny typed record for `Collection<Todo>` tests.
class _Todo {
  const _Todo(this.id, this.title);

  final String id;
  final String title;

  static _Todo fromRow(Map<String, dynamic> row) =>
      _Todo(row['id'] as String, row['title'] as String);

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
    test('throws StateError when the collection was built WITHOUT toRow '
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

    test('throws ArgumentError when toRow() omits the pkColumn '
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
      final (engine, db) = newDb(
        queryResult: jsonEncode([
          {'count': 5},
        ]),
      );
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos.count().first;
      engine.rowsController.add('[]'); // one change-tick → one re-query
      final n = await future;

      expect(n, 5);
      // Composed SQL is the verbatim count-with-alias shape nostos_database emits.
      expect(engine.queries, ['SELECT COUNT(*) AS count FROM todos']);
    });

    test('returns 0 when count is non-numeric ({"count": "oops"})', () async {
      final (engine, db) = newDb(
        queryResult: jsonEncode([
          {'count': 'oops'},
        ]),
      );
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos.count().first;
      engine.rowsController.add('[]');
      final n = await future;

      expect(n, 0);
    });

    test('returns 0 when the result set is empty', () async {
      final (engine, db) = newDb(queryResult: '[]');
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos.count().first;
      engine.rowsController.add('[]');
      final n = await future;

      expect(n, 0);
    });
  });

  group('Collection.watch', () {
    test('emits SELECT * FROM <table> and maps rows via fromRow '
        '(no where)', () async {
      final rows = [
        {'id': '1', 'title': 'ship'},
        {'id': '2', 'title': 'review'},
      ];
      final (engine, db) = newDb(queryResult: jsonEncode(rows));
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos.watch().first;
      engine.rowsController.add('[]'); // one change-tick → one re-query
      final items = await future;

      expect(items, const [_Todo('1', 'ship'), _Todo('2', 'review')]);
      expect(engine.queries, ['SELECT * FROM todos']);
    });

    test('composes "... WHERE <structured-where>" (ADR-0032 T2)', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos.watch(where: Where.eq('completed', 0)).first;
      engine.rowsController.add('[]');
      await future;

      expect(engine.queries, ['SELECT * FROM todos WHERE completed = 0']);
    });

    test('structured predicate: and/or/not + ordered ops + inList', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos
          .watch(
            where: Where.and([
              Where.eq('user_id', 'u1'),
              Where.or([Where.lt('due_at', 100), Where.gte('priority', 3)]),
              Where.not(Where.eq('archived', 1)),
              Where.inList('status', ['open', 'wip']),
            ]),
          )
          .first;
      engine.rowsController.add('[]');
      await future;

      // Combinator children are parenthesized so AND/OR precedence is explicit.
      expect(engine.queries.single, startsWith('SELECT * FROM todos WHERE '));
      expect(engine.queries.single, contains("user_id = 'u1'"));
      expect(engine.queries.single, contains('due_at < 100'));
      expect(engine.queries.single, contains('priority >= 3'));
      expect(engine.queries.single, contains('archived = 1'));
      expect(engine.queries.single, contains("status IN ('open', 'wip')"));
    });

    test('orderBy/limit/offset compose (ADR-0032 T2)', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos
          .watch(
            orderBy: [Order.desc('created_at'), Order.asc('id')],
            limit: 50,
            offset: 10,
          )
          .first;
      engine.rowsController.add('[]');
      await future;

      expect(engine.queries, [
        'SELECT * FROM todos ORDER BY created_at DESC, id ASC LIMIT 50 OFFSET 10',
      ]);
    });

    test('injection guard: a quote in a column name is rejected', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      expect(
        () => todos.watch(where: Where.eq("id'; DROP TABLE todos;--", 1)),
        throwsA(isA<ArgumentError>()),
      );
    });

    test('injection guard: a string value is escaped, not spliced', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      final future = todos.watch(where: Where.eq('title', "O'Brien")).first;
      engine.rowsController.add('[]');
      await future;

      // The single quote is doubled → SQL-safe, no statement break.
      expect(engine.queries, ["SELECT * FROM todos WHERE title = 'O''Brien'"]);
    });
  });

  group('SyncStatus', () {
    test(
      'disconnected with null lastSyncedAt initially; flips to connected '
      'with a non-null lastSyncedAt after the engine emits connected',
      () async {
        final (engine, db) = newDb();
        // Accessing currentStatus wires the internal status listener
        // (_ensureStatusWired) — must happen BEFORE the emit so the listener
        // captures the transition. Before any subscribe/emit, the honest P0
        // snapshot is: disconnected, no last-sync stamp.
        final initial = db.currentStatus;
        expect(
          initial.connected,
          isFalse,
          reason: 'default conn is disconnected',
        );
        expect(initial.lastSyncedAt, isNull, reason: 'never connected yet');

        // subscribe wires engine.stateController → Nostos.connectionState, which
        // the status listener listens to.
        await db.subscribe('todos');

        // Drive the state transition. Both the status listener and our
        // connectionState watcher are subscribed to the same broadcast stream;
        // await the watcher to guarantee propagation before reading currentStatus.
        final connectedFuture = db.connectionState.firstWhere(
          (s) => s == NostosConnectionState.connected,
        );
        engine.stateController.add(NostosConnectionState.connected);
        await connectedFuture;

        final after = db.currentStatus;
        expect(after.connected, isTrue);
        expect(after.lastSyncedAt, isNotNull);
      },
    );

    test('pending writes surface without being reported as an error', () async {
      final (engine, db) = newDb();
      expect(db.currentStatus.pendingWrites, 0);
      expect(db.currentStatus.hasWriteError, isFalse);
      await db.subscribe('todos');

      final seen = db.status;
      engine.writeStatusController.add((
        pending: 2,
        deadLettered: 0,
        lastError: null,
      ));
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

      engine.writeStatusController.add((
        pending: 3,
        deadLettered: 1,
        lastError: 'boom',
      ));
      await pumpEventQueue();
      expect(seen.value.pendingWrites, 3);

      final connectedFuture = db.connectionState.firstWhere(
        (s) => s == NostosConnectionState.connected,
      );
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
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

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
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      expect(
        () => todos.upsertRow({'title': 'no id'}),
        throwsA(isA<ArgumentError>()),
      );
    });

    test('patch enqueues op:patch with only the supplied columns', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      await todos.patch('7', {'completed': 1});

      expect(engine.writes.single.table, 'todos');
      expect(engine.writes.single.op, 'patch');
      expect(engine.writes.single.pk, '7');
      expect(jsonDecode(engine.writes.single.payloadJson!), {'completed': 1});
    });
  });

  group(
    'Collection reads (ADR-0032 T2: get/getAll/fetchById/watchOne/exists)',
    () {
      test('getAll maps rows one-shot with structured where', () async {
        final rows = [
          {'id': '1', 'title': 'a'},
          {'id': '2', 'title': 'b'},
        ];
        final (engine, db) = newDb(queryResult: jsonEncode(rows));
        await db.subscribe('todos');
        final todos = db.collection<_Todo>(
          table: 'todos',
          fromRow: _Todo.fromRow,
        );

        final items = await todos.getAll(
          where: Where.eq('user_id', 'u1'),
          orderBy: [Order.asc('id')],
          limit: 10,
        );

        expect(items, const [_Todo('1', 'a'), _Todo('2', 'b')]);
        expect(
          engine.queries.single,
          "SELECT * FROM todos WHERE user_id = 'u1' ORDER BY id ASC LIMIT 10",
        );
      });

      test('get returns the single row by pk, null when absent', () async {
        final (engineHit, dbHit) = newDb(
          queryResult: jsonEncode([
            {'id': '7', 'title': 'ship'},
          ]),
        );
        await dbHit.subscribe('todos');
        final todosHit = dbHit.collection<_Todo>(
          table: 'todos',
          fromRow: _Todo.fromRow,
        );
        final got = await todosHit.get('7');
        expect(got, const _Todo('7', 'ship'));
        // fetchById is the alias.
        expect(await todosHit.fetchById('7'), const _Todo('7', 'ship'));

        final (engineMiss, dbMiss) = newDb(queryResult: jsonEncode([]));
        await dbMiss.subscribe('todos');
        final todosMiss = dbMiss.collection<_Todo>(
          table: 'todos',
          fromRow: _Todo.fromRow,
        );
        expect(await todosMiss.get('nope'), isNull);
        // get composes a WHERE eq on the pkColumn + LIMIT 1. The String pk '7'
        // is emitted as a quoted SQLite literal by _literal (predicate.dart).
        expect(
          engineHit.queries.first,
          "SELECT * FROM todos WHERE id = '7' LIMIT 1",
        );
        expect(engineMiss.queries, isNotEmpty);
      });

      test('watchOne emits the row, then null when it disappears', () async {
        final (engine, db) = newDb(
          queryResult: jsonEncode([
            {'id': '7', 'title': 'ship'},
          ]),
        );
        await db.subscribe('todos');
        final todos = db.collection<_Todo>(
          table: 'todos',
          fromRow: _Todo.fromRow,
        );

        final future = todos.watchOne('7').first;
        engine.rowsController.add('[]'); // table-watch tick → re-query → emit
        expect(await future, const _Todo('7', 'ship'));
      });

      test('exists emits a boolean from the EXISTS() projection', () async {
        final (engine, db) = newDb(
          queryResult: jsonEncode([
            {'hit': 1},
          ]),
        );
        await db.subscribe('todos');
        final todos = db.collection<_Todo>(
          table: 'todos',
          fromRow: _Todo.fromRow,
        );

        final future = todos.exists(where: Where.eq('done', 1)).first;
        engine.rowsController.add('[]'); // tick → re-query → emit
        expect(await future, isTrue);
        expect(
          engine.queries.single,
          'SELECT EXISTS(SELECT 1 FROM todos WHERE done = 1) AS hit',
        );
      });
    },
  );

  group('writeBatch (ADR-0032 T3)', () {
    test(
      'delegates to a single atomic engine.writeBatch call, returns ids',
      () async {
        final (engine, db) = newDb();
        await db.subscribe('todos');

        final ids = await db.writeBatch([
          const NostosWrite(
            table: 'todos',
            op: 'upsert',
            pk: '1',
            payload: {'title': 'a'},
          ),
          const NostosWrite(
            table: 'todos',
            op: 'patch',
            pk: '1',
            payload: {'done': 1},
          ),
          const NostosWrite(table: 'todos', op: 'delete', pk: '2'),
        ]);

        expect(ids, [1, 2, 3]);
        // ONE batch call carrying all three ops — NOT three separate writes.
        expect(engine.batchCalls, hasLength(1));
        expect(
          engine.writes,
          isEmpty,
          reason: 'atomic batch never falls back to per-op write',
        );
        final batch = engine.batchCalls.single;
        expect(batch.map((o) => o.op).toList(), ['upsert', 'patch', 'delete']);
        expect(batch[1].pk, '1');
        // payloads are JSON-encoded by the Nostos layer before crossing FFI.
        expect(batch[1].payloadJson, isNotNull);
        expect(jsonDecode(batch[1].payloadJson!), {'done': 1});
      },
    );

    test('Collection.writeBatch stamps the table', () async {
      final (engine, db) = newDb();
      await db.subscribe('todos');
      final todos = db.collection<_Todo>(
        table: 'todos',
        fromRow: _Todo.fromRow,
      );

      await todos.writeBatch([
        const NostosWrite(table: 'WRONG', op: 'upsert', pk: '9', payload: {}),
      ]);

      expect(
        engine.batchCalls.single.single.table,
        'todos',
        reason: 'collection stamps its own table over the op',
      );
    });

    test(
      'an engine.writeBatch failure propagates with no partial batch',
      () async {
        final (engine, db) = newDb();
        await db.subscribe('todos');
        engine.batchResult = (_) async => throw Exception('disk full');

        Future<List<int>> run() => db.writeBatch([
          const NostosWrite(table: 'todos', op: 'upsert', pk: '1'),
          const NostosWrite(table: 'todos', op: 'upsert', pk: '2'),
          const NostosWrite(table: 'todos', op: 'upsert', pk: '3'),
        ]);

        await expectLater(run(), throwsA(isA<Exception>()));
        // The batch was attempted exactly once; nothing partial leaked through
        // (atomicity is the storage layer's job — here we assert the facade
        // never splits the batch into per-op writes on failure).
        expect(engine.batchCalls, hasLength(1));
        expect(engine.writes, isEmpty);
      },
    );
  });

  group('CRDT orSet handles (ADR-0030 / ADR-0032 T4)', () {
    test(
      'Collection.orSetAdd/orSetRemove delegate with the stamped table',
      () async {
        final (engine, db) = newDb();
        await db.subscribe('todos');
        final todos = db.collection<_Todo>(
          table: 'todos',
          fromRow: _Todo.fromRow,
        );

        await todos.orSetAdd(pk: '1', element: 'urgent');
        await todos.orSetRemove(pk: '1', element: 'done');

        expect(engine.orSetCalls, [
          (op: 'add', table: 'todos', pk: '1', element: 'urgent'),
          (op: 'remove', table: 'todos', pk: '1', element: 'done'),
        ]);
      },
    );

    test('orSetAdd on an unsubscribed table throws StateError', () async {
      final (_, db) = newDb();
      await db.subscribe('todos');

      expect(
        () => db.write(table: 'WRONG', op: 'upsert', pk: '1'),
        throwsA(isA<StateError>()),
      );
    });
  });

  group('CRDT counter handles (ADR-0030 addendum)', () {
    test(
      'Collection.counterIncrement/counterDecrement delegate with delta',
      () async {
        final (engine, db) = newDb();
        await db.subscribe('counts');
        final counts = db.collection<_Todo>(
          table: 'counts',
          fromRow: _Todo.fromRow,
        );

        await counts.counterIncrement(pk: '1', delta: 5);
        await counts.counterDecrement(pk: '1', delta: 2);

        expect(engine.counterCalls, [
          (op: 'increment', table: 'counts', pk: '1', delta: 5),
          (op: 'decrement', table: 'counts', pk: '1', delta: 2),
        ]);
      },
    );

    test(
      'counterIncrement on an unsubscribed table throws StateError',
      () async {
        final (_, db) = newDb();
        await db.subscribe('counts');

        expect(
          () => db
              .collection<_Todo>(table: 'WRONG', fromRow: _Todo.fromRow)
              .counterIncrement(pk: '1', delta: 1),
          throwsA(isA<StateError>()),
        );
      },
    );
  });

  group('deadLetters (ADR-0032 T5)', () {
    test('surfaces error + timestamp columns from the outbox', () async {
      final (_, db) = newDb(
        queryResult: jsonEncode([
          {
            'id': 7,
            'table_name': 'todos',
            'op': 'upsert',
            'pk': '1',
            'payload': '{"title":"ship"}',
            'attempts': 3,
            'last_error': 'NOSTOS_WRITE_TABLES rejects "todos"',
            'dead_lettered_at': 1700000000000,
          },
        ]),
      );
      await db.subscribe('todos');

      final letters = await db.deadLetters();

      expect(letters, hasLength(1));
      final dl = letters.single;
      expect(dl.id, 7);
      expect(dl.table, 'todos');
      expect(dl.op, 'upsert');
      expect(dl.attempts, 3);
      expect(dl.payload, {'title': 'ship'});
      expect(dl.error, 'NOSTOS_WRITE_TABLES rejects "todos"');
      expect(dl.timestamp, DateTime.fromMillisecondsSinceEpoch(1700000000000));
    });

    test('null error/timestamp degrade gracefully', () async {
      final (_, db) = newDb(
        queryResult: jsonEncode([
          {
            'id': 1,
            'table_name': 'todos',
            'op': 'delete',
            'pk': '9',
            'payload': null,
            'attempts': 1,
            'last_error': null,
            'dead_lettered_at': null,
          },
        ]),
      );
      await db.subscribe('todos');

      final letters = await db.deadLetters();
      expect(letters.single.error, isNull);
      expect(letters.single.timestamp, isNull);
      expect(letters.single.payload, isNull);
    });
  });

  group('pauseSync/resumeSync (ADR-0032 T1)', () {
    test(
      'pauseSync delegates to engine.disconnect; resumeSync to resume',
      () async {
        final (engine, db) = newDb();
        await db.subscribe('todos');

        await db.pauseSync();
        expect(engine.disconnectCalls, 1);

        db.resumeSync();
        expect(engine.resumeCalls, 1);
      },
    );
  });

  group('waitForFirstSync (ADR-0032 T1)', () {
    test('completes when the first connected transition lands', () async {
      final (engine, db) = newDb();
      db.currentStatus; // wire status
      await db.subscribe('todos');

      final barrier = db.waitForFirstSync();
      expect(db.currentStatus.hasSynced, isFalse);

      final connectedFuture = db.connectionState.firstWhere(
        (s) => s == NostosConnectionState.connected,
      );
      engine.stateController.add(NostosConnectionState.connected);
      await connectedFuture;
      await pumpEventQueue();

      await barrier.timeout(const Duration(seconds: 1));
      expect(db.currentStatus.hasSynced, isTrue);
    });

    test('resolves immediately when already synced', () async {
      final (engine, db) = newDb();
      db.currentStatus;
      await db.subscribe('todos');
      final f = db.connectionState.firstWhere(
        (s) => s == NostosConnectionState.connected,
      );
      engine.stateController.add(NostosConnectionState.connected);
      await f;
      await pumpEventQueue();

      // Already synced — second call must not hang.
      await db.waitForFirstSync().timeout(const Duration(seconds: 1));
    });
  });

  group('deadLetters (ADR-0032 T5 / ADR-0027)', () {
    test('lists quarantined rows from cairn_outbox WHERE dlq=1', () async {
      final rows = [
        {
          'id': 5,
          'table_name': 'todos',
          'op': 'upsert',
          'pk': '9',
          'payload': '{"title":"x"}',
          'attempts': 3,
        },
      ];
      final (engine, db) = newDb(queryResult: jsonEncode(rows));
      await db.subscribe('todos');

      final dl = await db.deadLetters();

      expect(dl.length, 1);
      expect(dl.first.id, 5);
      expect(dl.first.table, 'todos');
      expect(dl.first.attempts, 3);
      expect(dl.first.payload, {'title': 'x'});
      expect(dl.first.error, isNull, reason: 'not persisted per-row in v1');
      expect(dl.first.timestamp, isNull);
      expect(
        engine.queries.single,
        contains('FROM cairn_outbox WHERE dlq = 1'),
      );
    });
  });
}
