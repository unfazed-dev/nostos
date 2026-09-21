// `NostosDatabase.local` tests — the no-server entry point (free-tier parity:
// every nostos feature works identically with local-only storage; upgrade to
// sync later by reopening the SAME SQLite file with a URL — zero migration).
//
// Runs pure-Dart over a recording fake engine via Nostos.withEngine +
// NostosDatabase.localForTest — the same seams nostos_ws6_test.dart uses; no
// native library. The production `local()` wrapper is one Nostos.connect call
// around the path exercised here (covered end-to-end by the integration
// harness, where the real engine proves the paused loop never dials).
import 'dart:async';
import 'dart:convert';

import 'package:nostos_flutter/src/nostos.dart';
import 'package:nostos_flutter/src/nostos_database.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/schema.dart';
import 'package:flutter_test/flutter_test.dart';

const _schema = NostosSchema(
  tables: [
    NostosTable(
      name: 'tasks',
      primaryKey: ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('title'),
        NostosColumn.integer('completed'),
      ],
    ),
    NostosTable(
      name: 'notes',
      primaryKey: ['id'],
      columns: [NostosColumn.text('id'), NostosColumn.text('body')],
    ),
  ],
);

void main() {
  group('NostosDatabase.local', () {
    test('applies the declared schema and subscribes every declared table, then pauses sync', () async {
      final engine = _RecordingEngine();

      final db = await NostosDatabase.localForTest(
        Nostos.withEngine(engine),
        _schema,
      );

      // Schema applied BEFORE subscribing — the read-views exist before any
      // watch/query can hit them.
      expect(engine.appliedTables?.map((t) => t.name), ['tasks', 'notes']);
      expect(engine.subscribedTables, ['tasks', 'notes']);
      expect(
        engine.disconnectCalls,
        1,
        reason: 'pauseSync aborts the connect loop right after subscribe',
      );
      expect(engine.resumeCalls, 0);
      expect(
        engine.events.indexOf('applySchema'),
        lessThan(engine.events.indexOf('subscribe')),
      );
      expect(
        engine.events.indexOf('subscribe'),
        lessThan(engine.events.indexOf('disconnect')),
      );
      await db.close();
    });

    test('carries the CRDT table declarations into the subscribe', () async {
      final engine = _RecordingEngine();

      await NostosDatabase.localForTest(
        Nostos.withEngine(
          engine,
          orSetTables: const {'notes'},
          counterTables: const {'tasks'},
        ),
        _schema,
      );

      expect(engine.subscribedOrSetTables, {'notes'});
      expect(engine.subscribedCounterTables, {'tasks'});
    });

    test('reads and writes work through the paused session — the outbox is durable and watches pump', () async {
      final engine = _RecordingEngine(
        queryResult: jsonEncode([
          {'_pk': '1', 'title': 'ship', 'completed': false},
        ]),
      );
      final db = await NostosDatabase.localForTest(
        Nostos.withEngine(engine),
        _schema,
      );

      // A read against a declared table answers from local storage.
      final rows = await db.getAll('SELECT * FROM tasks');
      expect(rows.single['title'], 'ship');

      // A write enqueues into the durable outbox without any server.
      final outboxId = await db.write(
        table: 'tasks',
        op: 'upsert',
        pk: '1',
        payload: {'title': 'ship it'},
      );
      expect(outboxId, greaterThanOrEqualTo(0));

      // A watch attaches (the local open registered the subscription) instead
      // of throwing, and emits the local row set.
      final watched = await db
          .watch('SELECT * FROM tasks')
          .first
          .timeout(const Duration(seconds: 5));
      expect(watched, isA<List<Map<String, dynamic>>>());
      await db.close();
    });

    test('waitForFirstSync resolves immediately — no server means no first sync to wait for', () async {
      final db = await NostosDatabase.localForTest(
        Nostos.withEngine(_RecordingEngine()),
        _schema,
      );

      await db.waitForFirstSync().timeout(
        const Duration(seconds: 1),
        onTimeout: () =>
            fail('waitForFirstSync must not hang on a local database'),
      );
      await db.close();
    });

    test(
      'resumeSync fails loudly — a local database has no sync to resume',
      () async {
        final db = await NostosDatabase.localForTest(
          Nostos.withEngine(_RecordingEngine()),
          _schema,
        );

        expect(() => db.resumeSync(), throwsStateError);
        await db.close();
      },
    );

    test('push-token registration fails loudly — a local database has no server to knock', () async {
      final db = await NostosDatabase.localForTest(
        Nostos.withEngine(_RecordingEngine()),
        _schema,
      );

      expect(() => db.registerPushToken('fcm', 'tok'), throwsStateError);
      expect(() => db.deregisterPushToken('tok'), throwsStateError);
      await db.close();
    });

    test('an empty schema is rejected up front — there is no server to fetch one from', () async {
      expect(
        () => NostosDatabase.localForTest(
          Nostos.withEngine(_RecordingEngine()),
          const NostosSchema(tables: []),
        ),
        throwsArgumentError,
      );
    });
  });
}

/// A no-native-library [NostosEngine] that RECORDS the calls `local` makes so
/// the ordering (applySchema → subscribe → disconnect) is assertable. Serves
/// a canned query result so reads through the paused session have an answer.
class _RecordingEngine implements NostosEngine {
  _RecordingEngine({this.queryResult = '[]'});

  final String queryResult;

  /// Ordered call log: 'applySchema', 'subscribe', 'disconnect', 'resume'.
  final List<String> events = [];
  List<ClientTableFfi>? appliedTables;
  List<String>? subscribedTables;
  Set<String>? subscribedOrSetTables;
  Set<String>? subscribedCounterTables;
  int disconnectCalls = 0;
  int resumeCalls = 0;

  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();

  @override
  void applySchema(List<ClientTableFfi> tables) {
    events.add('applySchema');
    appliedTables = tables;
  }

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) {
    events.add('subscribe');
    subscribedTables = tables.map((t) => t.name).toList(growable: false);
    subscribedOrSetTables = orSetTables;
    subscribedCounterTables = counterTables;
    return const Stream<NostosConnectionState>.empty();
  }

  @override
  Stream<String> watch({required String table}) => Stream.value('[]');

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => const Stream.empty();

  @override
  Future<String> query({required String sql}) async => queryResult;

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
  }) async => 0;

  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async => List.filled(ops.length, 0);

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) async => 0;

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) async => 0;

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
  Future<void> setToken(String? token) async {}

  @override
  Future<void> disconnect() async {
    events.add('disconnect');
    disconnectCalls++;
  }

  @override
  Stream<NostosConnectionState> resume() {
    events.add('resume');
    resumeCalls++;
    return const Stream<NostosConnectionState>.empty();
  }

  @override
  Future<void> close() async {}

  @override
  Future<void> signOut() async {}
}
