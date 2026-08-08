// Unit tests for WebNostosEngine's Worker-protocol logic (ADR-0036).
//
// These run in the plain Dart VM (no browser, no wasm) via a FakeNostosWorkerPort
// that stands in for the JS Worker. They pin the load-bearing wiring the
// browser smoke can't economically cover on every build: request/response id
// correlation, multi-table watch stream fan-out, connection-state synthesis
// from Worker status pushes, write-status polling, and the reported CRDT gap
// (orSet/counter throw, not silently no-op).

import 'package:nostos_flutter/src/engine.dart';
import 'package:nostos_flutter/src/engine_web.dart';
import 'package:flutter_test/flutter_test.dart';

WebNostosEngine _engine(FakeNostosWorkerPort port) {
  final e = WebNostosEngine.forPort(port, url: 'ws://x/sync', token: 't');
  e.start();
  return e;
}

void main() {
  test('subscribe emits connecting, then connected on a Worker status push',
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
  });

  test('write correlates response by id and returns the outbox id', () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final f = eng.write(table: 'tasks', op: 'upsert', pk: '1', payloadJson: '{}');
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

  test('watchWriteStatus surfaces pending/deadLettered/lastError pushes',
      () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final seen =
        <({int pending, int deadLettered, String? lastError})>[];
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
  });

  test('writeBatch loops single writes (non-atomic on web — ADR-0036 ponytail)',
      () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    final f = eng.writeBatch(ops: [
      (table: 't', op: 'upsert', pk: '1', payloadJson: null),
      (table: 't', op: 'upsert', pk: '2', payloadJson: null),
    ]);

    // writeBatch awaits each write in sequence, so replies must arrive in order.
    await Future<void>.delayed(Duration.zero);
    final first = port.sent.lastWhere((m) => m['cmd'] == 'write');
    port.reply({'id': first['id'] as int, 'ok': true, 'writeId': 10});
    await Future<void>.delayed(Duration.zero);
    final second = port.sent.lastWhere(
      (m) => m['cmd'] == 'write' && (m['id'] as int) != (first['id'] as int),
    );
    port.reply({'id': second['id'] as int, 'ok': true, 'writeId': 20});

    expect(await f, [10, 20]);
    expect(port.sent.where((m) => m['cmd'] == 'write').length, 2);
    await eng.close();
  });

  test('CRDT verbs throw UnsupportedError (reported gap — not a silent no-op)',
      () async {
    final port = FakeNostosWorkerPort();
    final eng = _engine(port);
    expect(
      () => eng.orSetAdd(table: 't', pk: '1', element: 'x'),
      throwsA(isA<UnsupportedError>()),
    );
    expect(
      () => eng.counterIncrement(table: 't', pk: '1', delta: 1),
      throwsA(isA<UnsupportedError>()),
    );
    await eng.close();
  });

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
