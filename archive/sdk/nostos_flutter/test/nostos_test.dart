// Unit tests for the Nostos public API surface, against a fake NostosEngine —
// no native library involved (see lib/src/engine.dart's doc for why the
// NostosEngine seam exists). Covers subscribe/watch/write wiring, the
// single-table-per-instance constraint, and JSON decode of the rows stream.

import 'dart:async';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:fake_async/fake_async.dart';
import 'package:flutter_test/flutter_test.dart';

class FakeNostosEngine implements NostosEngine {
  final rowsController = StreamController<String>.broadcast();
  final stateController = StreamController<NostosConnectionState>.broadcast();

  List<NostosTableSub> lastTables = [];
  int subscribeCallCount = 0;

  /// Convenience accessors preserving the single-table test shape.
  String? get lastSubscribedTable =>
      lastTables.isEmpty ? null : lastTables.first.name;
  String? get lastWhereSql =>
      lastTables.isEmpty ? null : lastTables.first.whereSql;

  final List<({String table, String op, String pk, String? payloadJson})>
  writes = [];
  int nextWriteId = 1;
  int closeCallCount = 0;

  /// The JSON-array string [query] returns. Tests set this to control the
  /// decoded result [watchQuery] emits. Defaults to an empty result set.
  String queryResult = '[]';
  final List<String> queries = [];

  @override
  Future<String> query({required String sql}) async {
    queries.add(sql);
    return queryResult;
  }

  @override
  Stream<NostosConnectionState> subscribe({required List<NostosTableSub> tables}) {
    subscribeCallCount++;
    lastTables = tables;
    return stateController.stream;
  }

  @override
  Stream<String> watch({required String table}) => rowsController.stream;

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
      watchWriteStatus() => const Stream.empty();

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    writes.add((table: table, op: op, pk: pk, payloadJson: payloadJson));
    return nextWriteId++;
  }

  @override
  void applySchema(List<ClientTableFfi> tables) {
    // Stub: record nothing for now. Tests that exercise applySchema wiring
    // can assert on this fake's captured tables once added (WS3 follow-up).
  }

  @override
  Future<void> close() async {
    closeCallCount++;
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

  @override
  Future<void> disconnect() async {}

  @override
  Stream<NostosConnectionState> resume() => stateController.stream;
}

/// Mirrors the real FFI `watch()`: pushes an initial row snapshot when its
/// stream is first listened to (the `emit_snapshot` path). The passive
/// [FakeNostosEngine.watch] (a plain broadcast that never emits on subscribe)
/// can't reproduce the P0-3 regression; this fake can.
class SnapshotOnSubscribeEngine implements NostosEngine {
  SnapshotOnSubscribeEngine({this.queryResult = '[{"id":"1","name":"Alpha"}]'});

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
      watchWriteStatus() => const Stream.empty();

  final stateController = StreamController<NostosConnectionState>.broadcast();
  final List<String> queries = [];
  String queryResult;
  int _nextWriteId = 1;

  @override
  Stream<NostosConnectionState> subscribe({required List<NostosTableSub> tables}) =>
      stateController.stream;

  @override
  Stream<String> watch({required String table}) {
    // Emit the initial snapshot ON FIRST LISTEN (mirrors emit_snapshot). A
    // fresh single-subscription controller per call so the snapshot fires once
    // per subscription, exactly like the FFI pump.
    late final StreamController<String> c;
    c = StreamController<String>(
      onListen: () =>
          scheduleMicrotask(() => c.add('[{"_pk":"snap-$table"}]')),
    );
    return c.stream;
  }

  @override
  Future<String> query({required String sql}) async {
    queries.add(sql);
    return queryResult;
  }

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async =>
      _nextWriteId++;

  @override
  void applySchema(List<ClientTableFfi> tables) {}

  @override
  Future<void> close() async {}

  /// Recorded so a test can assert `Nostos.setToken` actually delegates —
  /// a silently-dropped refresh is the exact bug this seam exists to prevent.
  String? lastSetToken;
  int setTokenCalls = 0;

  @override
  Future<void> setToken(String? token) async {
    lastSetToken = token;
    setTokenCalls++;
  }

  @override
  Future<void> disconnect() async {}

  @override
  Stream<NostosConnectionState> resume() => stateController.stream;
}

void main() {
  group('Nostos.subscribe/watch', () {
    test('watch() before subscribe() throws StateError', () {
      final nostos = Nostos.withEngine(FakeNostosEngine());
      expect(() => nostos.watch('tasks'), throwsStateError);
    });

    test('subscribe() then watch() decodes the JSON row stream', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);

      await nostos.subscribe('tasks', where: 'status = open');
      expect(engine.lastSubscribedTable, 'tasks');
      expect(engine.lastWhereSql, 'status = open');

      final future = nostos.watch('tasks').first;
      engine.rowsController.add('[{"_pk":"1","title":"a"}]');
      final rows = await future;
      expect(rows, [
        {'_pk': '1', 'title': 'a'},
      ]);
    });

    test('watch() for a different table than subscribed throws', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');
      expect(() => nostos.watch('notes'), throwsStateError);
    });

    test('a second subscribe() call replaces the first', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');
      await nostos.subscribe('notes');
      expect(engine.subscribeCallCount, 2);
      expect(() => nostos.watch('tasks'), throwsStateError);
      expect(() => nostos.watch('notes'), returnsNormally);
    });

    test('connectionState forwards engine state transitions', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');

      final future = nostos.connectionState.first;
      engine.stateController.add(NostosConnectionState.connected);
      expect(await future, NostosConnectionState.connected);
    });
  });

  group('Nostos.watchQuery', () {
    test('watchQuery() before subscribe() throws StateError', () {
      final nostos = Nostos.withEngine(FakeNostosEngine());
      expect(
        () => nostos.watchQuery('SELECT 1'),
        throwsStateError,
      );
    });

    test('watchQuery() re-runs SQL on each change tick and decodes rows',
        () async {
      final engine = FakeNostosEngine()
        ..queryResult =
            '[{"title":"buy milk"},{"title":"ship nostos"}]';
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');

      const sql =
          "SELECT json_extract(payload, '\$.title') AS title FROM cairn_data";
      final rowsFuture = nostos.watchQuery(sql).take(2).toList();

      // The change-tick pump: watchQuery is wired to the same row stream
      // `watch(table)` uses, so each emitted snapshot retriggers the SQL.
      engine.rowsController.add('[{"_pk":"1"}]');
      engine.rowsController.add('[{"_pk":"1"},{"_pk":"2"}]');

      final rows = await rowsFuture;
      expect(rows, [
        [
          {'title': 'buy milk'},
          {'title': 'ship nostos'},
        ],
        [
          {'title': 'buy milk'},
          {'title': 'ship nostos'},
        ],
      ]);
      // One query call per tick — confirms reactivity, not a one-shot.
      expect(engine.queries, [sql, sql]);
    });

    test(
      'watchQuery(triggerOnTables: [active]) accepts the subscribed table',
      () async {
        final engine = FakeNostosEngine();
        final nostos = Nostos.withEngine(engine);
        await nostos.subscribe('tasks');
        expect(
          () => nostos.watchQuery('SELECT 1', triggerOnTables: ['tasks']),
          returnsNormally,
        );
      },
    );

    test(
      'watchQuery(triggerOnTables: [other]) rejects a non-subscribed table',
      () async {
        final engine = FakeNostosEngine();
        final nostos = Nostos.withEngine(engine);
        await nostos.subscribe('tasks');
        expect(
          () => nostos.watchQuery('SELECT 1', triggerOnTables: ['other']),
          throwsArgumentError,
        );
      },
    );

    test(
      'watchQuery(throttle) coalesces a burst of ticks into one re-query',
      () {
        // FakeAsync is re-exported by flutter_test. The throttle Timer is a
        // fake timer — it only fires when we elapse fake time. Microtask-
        // based awaits (FakeNostosEngine.subscribe/query) flush transparently.
        FakeAsync().run((fake) async {
          final engine = FakeNostosEngine()..queryResult = '[]';
          final nostos = Nostos.withEngine(engine);
          await nostos.subscribe('tasks');

          final emitted = <List<Map<String, dynamic>>>[];
          nostos
              .watchQuery(
                'SELECT 1',
                throttle: const Duration(milliseconds: 100),
              )
              .listen(emitted.add);

          // 5 rapid ticks, all within the 100ms throttle window — the
          // trailing edge must NOT have fired yet.
          for (var i = 0; i < 5; i++) {
            engine.rowsController.add('[]');
          }
          expect(engine.queries, isEmpty);

          fake.elapse(const Duration(milliseconds: 100));

          // After the window closes: exactly ONE query for the whole
          // burst — PowerSync's throttle coalesce contract (N rapid ticks
          // → ≤1 re-query).
          expect(engine.queries.length, 1);
          expect(emitted.length, 1);
        });
      },
    );

    test(
      'watchQuery delivers the initial snapshot to a LATE downstream listener '
      '(P0-3 regression: _mergeTriggers must subscribe lazily)',
      () async {
        // Two subscribed tables => watchQuery's default triggers hit the MERGE
        // path (sources.length > 1), not the single-source fast path. The
        // SnapshotOnSubscribeEngine pushes an initial snapshot when its stream
        // is first listened to (mirrors the FFI emit_snapshot).
        final engine = SnapshotOnSubscribeEngine();
        final nostos = Nostos.withEngine(engine);
        await nostos.subscribeTables(const [
          NostosTableSub(name: 'tasks'),
          NostosTableSub(name: 'notes'),
        ]);

        // Build the stream (what a StreamBuilder does in build()), then yield
        // one event-loop turn BEFORE subscribing. With the eager-merge bug this
        // is exactly the firing window: the engine's subscribe-time snapshot
        // flows into the merge controller while the downstream StreamBuilder
        // has not mounted yet, so it is dropped into a broadcast with no
        // listener — "No providers yet." with rows on disk.
        final stream = nostos.watchQuery('SELECT * FROM tasks');
        await Future<void>.delayed(Duration.zero);

        // The late subscription (a StreamBuilder mounts on the next frame).
        // With the lazy-merge fix, subscribing here wires the upstreams, the
        // engine pushes its snapshot in response, and it reaches this listener.
        final rows = await stream
            .take(1)
            .timeout(const Duration(seconds: 1))
            .toList();

        expect(rows, hasLength(1));
        expect(rows.single.single['name'], 'Alpha');
        expect(engine.queries, isNotEmpty,
            reason: 'the initial snapshot tick must re-run the SQL');
      },
    );
  });

  group('Nostos.write', () {
    test('write() before subscribe() throws StateError', () {
      final nostos = Nostos.withEngine(FakeNostosEngine());
      expect(
        () => nostos.write('tasks', op: 'upsert', pk: '1'),
        throwsStateError,
      );
    });

    test('write() for a table other than the active subscription throws', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');
      expect(
        () => nostos.write('notes', op: 'upsert', pk: '1'),
        throwsStateError,
      );
    });

    test('write() encodes the payload as JSON and returns the outbox id', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');

      final id = await nostos.write(
        'tasks',
        op: 'upsert',
        pk: '42',
        payload: {'title': 'buy milk'},
      );

      expect(id, 1);
      expect(engine.writes, [
        (
          table: 'tasks',
          op: 'upsert',
          pk: '42',
          payloadJson: '{"title":"buy milk"}',
        ),
      ]);
    });

    test('write() with no payload passes payloadJson: null (delete)', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');

      await nostos.write('tasks', op: 'delete', pk: '42');

      expect(engine.writes.single.payloadJson, isNull);
      expect(engine.writes.single.op, 'delete');
    });
  });

  group('Nostos.close', () {
    test('close() tears down the engine and is safe to call twice', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');

      await nostos.close();
      expect(engine.closeCallCount, 1);

      // Idempotent — a second call must not throw.
      await nostos.close();
      expect(engine.closeCallCount, 2);
    });

    test('close() is safe with no prior subscribe()', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);

      await nostos.close();
      expect(engine.closeCallCount, 1);
    });
  });

  group('Nostos.setToken', () {
    // The bug this guards: a Supabase JWT expires in ~1h, the server enforces
    // `exp`, and the reconnect loop re-sends whatever token it holds. If the
    // refresh never reaches the engine, sync dies silently and the UI keeps
    // showing local rows. Delegation is the whole contract.
    test('forwards the new token to the engine', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);

      await nostos.setToken('fresh-jwt');

      expect(engine.setTokenCalls, 1);
      expect(engine.lastSetToken, 'fresh-jwt');
    });

    test('forwards null to clear the token on sign-out', () async {
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);

      await nostos.setToken(null);

      expect(engine.setTokenCalls, 1);
      expect(engine.lastSetToken, isNull);
    });

    test('does not disturb an active watch stream', () async {
      // setToken must be non-destructive: the pure-Dart alternative was to
      // rebuild the handle, which ends every watch stream (`_replayLatest` wires
      // `onDone: controller.close`) and would look to the user like data
      // vanishing an hour after login.
      final engine = FakeNostosEngine();
      final nostos = Nostos.withEngine(engine);
      await nostos.subscribe('tasks');
      final seen = <List<Map<String, dynamic>>>[];
      var streamDone = false;
      nostos.watch('tasks').listen(seen.add, onDone: () => streamDone = true);

      engine.rowsController.add('[{"pk":"1","title":"before"}]');
      await Future<void>.delayed(Duration.zero);
      await nostos.setToken('fresh-jwt');
      engine.rowsController.add('[{"pk":"1","title":"after"}]');
      await Future<void>.delayed(Duration.zero);

      expect(streamDone, isFalse, reason: 'setToken must not close watch streams');
      expect(seen.length, 2, reason: 'rows must keep flowing across a refresh');
      expect(seen.last.first['title'], 'after');
    });
  });
}
