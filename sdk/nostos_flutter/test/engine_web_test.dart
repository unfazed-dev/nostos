// Unit tests for WebNostosEngine's Worker-protocol logic (ADR-0036).
//
// These run in the plain Dart VM (no browser, no wasm) via a FakeNostosWorkerPort
// that stands in for the JS Worker. They pin the load-bearing wiring the
// browser smoke can't economically cover on every build: request/response id
// correlation, multi-table watch stream fan-out, connection-state synthesis
// from Worker status pushes, write-status polling, and (Wave 4c) the CRDT +
// atomic-writeBatch Worker commands.

import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/engine_web.dart';
import 'package:flutter_test/flutter_test.dart';

WebNostosEngine _engine(FakeNostosWorkerPort port) {
  final e = WebNostosEngine.forPort(port, url: 'ws://x/sync', token: 't');
  e.start();
  return e;
}

void main() {
  test(
    'signOut waits for the Worker to acknowledge the durable wipe',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = _engine(port);
      var finished = false;
      final signingOut = eng.signOut().then((_) => finished = true);
      await Future<void>.delayed(Duration.zero);
      final request = port.sent.single;
      expect(request['cmd'], 'signOut');
      expect(finished, isFalse);
      port.reply({'id': request['id'], 'ok': true});
      await signingOut;
      expect(finished, isTrue);
    },
  );

  test('signOut surfaces a failed Worker wipe', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final signingOut = expectLater(eng.signOut(), throwsStateError);
    await Future<void>.delayed(Duration.zero);
    final request = port.sent.single;
    port.reply({'id': request['id'], 'error': 'OPFS wipe failed'});
    await signingOut;
    final retry = eng.signOut();
    final retryRequest = port.sent.last;
    expect(retryRequest['cmd'], 'signOut');
    port.reply({'id': retryRequest['id'], 'ok': true});
    await retry;
  });

  test('late storage subscriber receives the already reported mode', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    port.reply({'type': 'storage', 'mode': 'memory'});
    await Future<void>.delayed(Duration.zero);
    expect(await eng.webStorageDegraded.first, isTrue);
    await eng.close();
  });

  test('Appwrite token refresh and pause/resume reach the Worker', () async {
    final port = FakeNostosWorkerPort();
    final eng = WebNostosEngine.forPort(
      port,
      url: 'https://cloud.example/v1',
      token: 'old',
      appwrite: (projectId: 'project', functionId: 'sync', userId: 'alice'),
    )..start();
    final refreshed = eng.setToken('new');
    final tokenRequest = port.sent.last;
    expect(tokenRequest['cmd'], 'setToken');
    expect(tokenRequest['token'], 'new');
    port.reply({'id': tokenRequest['id'], 'ok': true});
    await refreshed;

    final paused = eng.disconnect();
    final pauseRequest = port.sent.last;
    expect(pauseRequest['cmd'], 'disconnect');
    port.reply({'id': pauseRequest['id'], 'ok': true});
    await paused;

    eng.resume();
    final resumeRequest = port.sent.last;
    expect(resumeRequest['cmd'], 'resume');
    port.reply({'id': resumeRequest['id'], 'ok': true});
    await eng.close();
  });

  test(
    'Appwrite connect carries cloud identity and revoked status stays distinct',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = WebNostosEngine.forPort(
        port,
        url: 'https://cloud.example/v1',
        token: 'short-lived-jwt',
        appwrite: (
          projectId: 'project',
          functionId: 'atlet_sync',
          userId: 'alice',
        ),
      )..start();
      final states = <NostosConnectionState>[];
      final sub = eng.subscribe(tables: const [NostosTableSub(name: 'orders')]);
      final done = sub.listen(states.add);
      await Future<void>.delayed(Duration.zero);
      final connect = port.sent.single;
      expect(connect['provider'], 'appwrite');
      expect(connect['projectId'], 'project');
      expect(connect['functionId'], 'atlet_sync');
      expect(connect['userId'], 'alice');
      expect(connect['token'], 'short-lived-jwt');
      port.reply({'type': 'status', 'connected': false, 'accessRevoked': true});
      await Future<void>.delayed(Duration.zero);
      expect(states.last, NostosConnectionState.accessRevoked);
      await done.cancel();
      await eng.close();
    },
  );

  test(
    'subscribe emits connecting, then connected on a Worker status push',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = _engine(port);
      final states = <NostosConnectionState>[];
      final sub = eng.subscribe(tables: const [NostosTableSub(name: 'tasks')]);
      final done = sub.listen(states.add);

      // Let the connect request flush, then push connected.
      await Future<void>.delayed(Duration.zero);
      expect(port.sent.single['cmd'], 'connect');
      port.reply({'type': 'status', 'connected': true});
      await Future<void>.delayed(Duration.zero);

      expect(states, [
        NostosConnectionState.connecting,
        NostosConnectionState.connected,
      ]);
      done.cancel();
      await eng.close();
    },
  );

  test('subscribe threads CRDT tables into the connect command (T4 config surface)', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    eng.subscribe(
      tables: const [NostosTableSub(name: 'tasks')],
      orSetTables: const {'tags'},
      counterTables: const {'likes'},
    );
    await Future<void>.delayed(Duration.zero);

    // The connect cmd must carry the CRDT-table tags so the Worker re-tags on
    // every (re)connect (nostos_worker.js openSocket → setCrdtTables). Without
    // this, orSet/counter verbs throw *TableNotTagged on web.
    final req = port.sent.single;
    expect(req['cmd'], 'connect');
    expect(req['orSetTables'], ['tags']);
    expect(req['counterTables'], ['likes']);
    await eng.close();
  });

  test(
    'storage push maps mode+reason to NostosWebStorageMode and persisted',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = _engine(port);
      final degraded = <bool>[];
      final sub = eng.webStorageDegraded.listen(degraded.add);

      port.reply({'type': 'storage', 'mode': 'durable', 'persisted': true});
      await Future<void>.delayed(Duration.zero);
      expect(eng.storageMode, NostosWebStorageMode.durable);
      expect(eng.storagePersisted, true);

      // A second tab of the same origin: the Worker lost the OPFS leader lock.
      port.reply({
        'type': 'storage',
        'mode': 'memory',
        'reason': 'secondary-tab',
        'persisted': false,
      });
      await Future<void>.delayed(Duration.zero);
      expect(eng.storageMode, NostosWebStorageMode.secondaryTab);

      // Plain OPFS degrade (Safari Private Browsing) stays `memory`.
      port.reply({
        'type': 'storage',
        'mode': 'memory',
        'reason': 'opfs-unavailable',
      });
      await Future<void>.delayed(Duration.zero);
      expect(eng.storageMode, NostosWebStorageMode.memory);
      expect(eng.storagePersisted, isNull);

      expect(degraded, [false, true, true]);
      await sub.cancel();
      await eng.close();
    },
  );

  test('write correlates response by id and returns the outbox id', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final f = eng.write(
      table: 'tasks',
      op: 'upsert',
      pk: '1',
      payloadJson: '{}',
    );
    await Future<void>.delayed(Duration.zero);

    final req = port.sent.single;
    expect(req['cmd'], 'write');
    final id = req['id'] as int;
    port.reply({'id': id, 'ok': true, 'writeId': 42});

    expect(await f, 42);
    await eng.close();
  });

  test('watch fans per-table snapshots to the right stream', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final tasksJson = <String>[];
    final usersJson = <String>[];
    eng.watch(table: 'tasks').listen(tasksJson.add);
    eng.watch(table: 'users').listen(usersJson.add);
    await Future<void>.delayed(Duration.zero);
    port.sent.clear();

    port.reply({'type': 'snapshot', 'table': 'tasks', 'json': '[{"id":1}]'});
    port.reply({'type': 'snapshot', 'table': 'users', 'json': '[{"id":"a"}]'});
    await Future<void>.delayed(Duration.zero);

    expect(tasksJson, ['[{"id":1}]']);
    expect(usersJson, ['[{"id":"a"}]']);
    await eng.close();
  });

  test(
    'watchWriteStatus surfaces pending/deadLettered/lastError pushes',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = _engine(port);
      final seen = <({int pending, int deadLettered, String? lastError})>[];
      eng.watchWriteStatus().listen(seen.add);

      port.reply({
        'type': 'writeStatus',
        'pending': 3,
        'deadLettered': 1,
        'lastError': 'boom',
      });
      await Future<void>.delayed(Duration.zero);

      expect(seen.last, (pending: 3, deadLettered: 1, lastError: 'boom'));
      await eng.close();
    },
  );

  test(
    'writeBatch sends a single atomic writeBatch command (Wave 4c)',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = _engine(port);
      final f = eng.writeBatch(
        ops: [
          (table: 't', op: 'upsert', pk: '1', payloadJson: null),
          (table: 't', op: 'upsert', pk: '2', payloadJson: null),
        ],
      );

      // Wave 4c: one writeBatch request (not a loop of single writes).
      await Future<void>.delayed(Duration.zero);
      final req = port.sent.singleWhere((m) => m['cmd'] == 'writeBatch');
      expect(req['ops'], [
        {'table': 't', 'op': 'upsert', 'pk': '1', 'payloadJson': null},
        {'table': 't', 'op': 'upsert', 'pk': '2', 'payloadJson': null},
      ]);
      final id = req['id'] as int;
      port.reply({
        'id': id,
        'ok': true,
        'writeIds': [10, 20],
      });

      expect(await f, [10, 20]);
      // Exactly one Worker request — no per-op write fan-out.
      expect(port.sent.where((m) => m['cmd'] == 'write'), isEmpty);
      await eng.close();
    },
  );

  test('orSetAdd wires to the orSetAdd Worker command (Wave 4c)', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final f = eng.orSetAdd(table: 'tags', pk: 'row1', element: 'alice');
    await Future<void>.delayed(Duration.zero);
    final req = port.sent.singleWhere((m) => m['cmd'] == 'orSetAdd');
    expect(req['table'], 'tags');
    expect(req['pk'], 'row1');
    expect(req['element'], 'alice');
    port.reply({'id': req['id'] as int, 'ok': true, 'writeId': 7});
    expect(await f, 7);
    await eng.close();
  });

  test(
    'counterIncrement + counterDecrement wire to Worker commands (Wave 4c)',
    () async {
      final port = FakeNostosWorkerPort();
      final eng = _engine(port);

      final inc = eng.counterIncrement(table: 'likes', pk: 'p1', delta: 5);
      await Future<void>.delayed(Duration.zero);
      final incReq = port.sent.singleWhere(
        (m) => m['cmd'] == 'counterIncrement',
      );
      expect(incReq['delta'], 5);
      port.reply({'id': incReq['id'] as int, 'ok': true, 'writeId': 11});
      expect(await inc, 11);

      final dec = eng.counterDecrement(table: 'likes', pk: 'p1', delta: 2);
      await Future<void>.delayed(Duration.zero);
      final decReq = port.sent.singleWhere(
        (m) => m['cmd'] == 'counterDecrement',
      );
      expect(decReq['delta'], 2);
      port.reply({'id': decReq['id'] as int, 'ok': true, 'writeId': 12});
      expect(await dec, 12);
      await eng.close();
    },
  );

  test('query returns the Worker json (defaults to [] when absent)', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final f = eng.query(sql: 'SELECT * FROM tasks');
    await Future<void>.delayed(Duration.zero);
    final id = port.sent.single['id'] as int;
    port.reply({'id': id, 'ok': true, 'json': '[{"id":1}]'});
    expect(await f, '[{"id":1}]');
    await eng.close();
  });

  test('close terminates the (owned) port and closes all streams', () async {
    final port = FakeNostosWorkerPort();
    // connect() marks the Worker as owned, so close() terminates the port.
    final eng = WebNostosEngine.connect(port: port, url: 'ws://x/sync');
    eng.start();
    await eng.close();
    expect(port.messages, emitsDone);
  });
}
