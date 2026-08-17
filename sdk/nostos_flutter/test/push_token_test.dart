// ADR-0037 §3 / plan task 4.1 — push-token REST registration.
//
// Pure-Dart: a fake NostosEngine (no native library, the attachments_test
// pattern) + a local dart:io HttpServer standing in for the pinned server
// contract:
//
//   POST /push-tokens        {"platform":"fcm"|"apns"|"webpush","token":"…"}
//                            auth: same JWT as /sync → 204
//   DELETE /push-tokens/{token}   same auth → 204
//
// The server routes are built against the SAME pinned contract; these tests
// pin the SDK's wire shape (method, path, headers, exact JSON body) so any
// drift fails here first. tenant/account are never sent — the server stamps
// them (ADR-0018 discipline).

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:nostos_flutter/src/engine.dart';
import 'package:flutter_test/flutter_test.dart';

/// Fake NostosEngine — every method is a harmless stub; only `signOut` is
/// observed (the sign-out-hook test asserts the core wipe ran too). Same
/// shape as attachments_test's `_AttachFakeEngine`.
class _PushFakeEngine implements NostosEngine {
  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();

  final stateController = StreamController<NostosConnectionState>.broadcast();

  int signOutCalls = 0;
  @override
  Future<void> signOut() async {
    signOutCalls++;
  }

  // ──────────────────── unused-but-required NostosEngine surface ────────────────────
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
  Stream<String> watch({required String table}) => const Stream<String>.empty();
  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => const Stream.empty();
  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async => 1;
  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async => List.generate(ops.length, (i) => i + 1);
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
  }) async => 1;
  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) async => 1;
  @override
  Future<String> query({required String sql}) async => '[]';
  @override
  void applySchema(List<ClientTableFfi> tables) {}
  @override
  Future<void> setToken(String? token) async {}
  @override
  Future<void> disconnect() async {}
  @override
  Stream<NostosConnectionState> resume() => stateController.stream;
  @override
  Future<void> close() async {
    await stateController.close();
  }
}

/// One captured request: method, path, lowercased headers, verbatim body.
class _Captured {
  const _Captured(this.method, this.path, this.headers, this.body);
  final String method;
  final String path;
  final Map<String, String> headers;
  final String body;
}

/// Local server capturing every request and replying [status] (+ [body]).
Future<(HttpServer, List<_Captured>)> _startServer(
  int status, {
  String body = '',
}) async {
  final server = await HttpServer.bind('127.0.0.1', 0);
  final requests = <_Captured>[];
  server.listen((req) async {
    final reqBody = await utf8.decoder.bind(req).join();
    final headers = <String, String>{};
    req.headers.forEach((name, values) {
      headers[name.toLowerCase()] = values.first;
    });
    requests.add(_Captured(req.method, req.uri.path, headers, reqBody));
    req.response.statusCode = status;
    if (body.isNotEmpty) req.response.write(body);
    await req.response.close();
  });
  return (server, requests);
}

NostosDatabase _newDb(HttpServer server, {String? token = 'jwt-abc'}) =>
    NostosDatabase.forTest(
      Nostos.withEngine(_PushFakeEngine()),
      const NostosSchema(tables: []),
      httpBase: 'http://127.0.0.1:${server.port}',
      token: token,
    );

/// First /push-tokens request 401s, the rest 204 — the cold-restored
/// stale-session shape that once stranded an emulator out of the push
/// registry (ADR-0037 pilot).
Future<(HttpServer, List<_Captured>)> _start401Then204Server() async {
  final server = await HttpServer.bind('127.0.0.1', 0);
  final requests = <_Captured>[];
  server.listen((req) async {
    final reqBody = await utf8.decoder.bind(req).join();
    final headers = <String, String>{};
    req.headers.forEach((name, values) {
      headers[name.toLowerCase()] = values.first;
    });
    requests.add(_Captured(req.method, req.uri.path, headers, reqBody));
    req.response.statusCode =
        requests.length == 1 ? 401 : 204; // first is the stale token
    await req.response.close();
  });
  return (server, requests);
}

void main() {
  test('a 401 triggers one session refresh and a retry — registration '
      'self-heals instead of stranding the device', () async {
    final (server, requests) = await _start401Then204Server();
    var refreshCalls = 0;
    final db = NostosDatabase.forTest(
      Nostos.withEngine(_PushFakeEngine()),
      const NostosSchema(tables: []),
      httpBase: 'http://127.0.0.1:${server.port}',
      token: 'stale-jwt',
      sessionRefresh: () async {
        refreshCalls++;
        return 'fresh-jwt';
      },
    );

    await db.registerPushToken('fcm', 'tok-123');

    expect(requests, hasLength(2));
    expect(refreshCalls, 1);
    expect(requests.first.headers['authorization'], 'Bearer stale-jwt');
    expect(requests.last.headers['authorization'], 'Bearer fresh-jwt',
        reason: 'the retry must re-read the credential, not replay it');
    await server.close();
  });

  test('registerPushToken POSTs the exact JSON body to /push-tokens '
      'with the sync JWT as Bearer', () async {
    final (server, requests) = await _startServer(204);
    final db = _newDb(server);

    await db.registerPushToken('fcm', 'tok-123');

    expect(requests, hasLength(1));
    final r = requests.single;
    expect(r.method, 'POST');
    expect(r.path, '/push-tokens');
    expect(r.headers['authorization'], 'Bearer jwt-abc');
    expect(r.headers['content-type'], startsWith('application/json'));
    expect(r.body, '{"platform":"fcm","token":"tok-123"}');
    await server.close();
  });

  test(
    'deregisterPushToken DELETEs the token path with the same auth',
    () async {
      final (server, requests) = await _startServer(204);
      final db = _newDb(server);

      await db.registerPushToken('apns', 'apns-tok');
      await db.deregisterPushToken('apns-tok');

      expect(requests, hasLength(2));
      final r = requests.last;
      expect(r.method, 'DELETE');
      expect(r.path, '/push-tokens/apns-tok');
      expect(r.headers['authorization'], 'Bearer jwt-abc');
      await server.close();
    },
  );

  test(
    'a non-204 reply throws NostosPushTokenException and registers nothing',
    () async {
      final (server, requests) = await _startServer(
        401,
        body: '{"error":"unauthorized"}',
      );
      final db = _newDb(server);

      await expectLater(
        db.registerPushToken('fcm', 'tok-123'),
        throwsA(
          isA<NostosPushTokenException>()
              .having((e) => e.statusCode, 'statusCode', 401)
              .having((e) => e.operation, 'operation', 'register')
              .having((e) => e.body, 'body', '{"error":"unauthorized"}'),
        ),
      );

      // The failed registration must not be tracked — signOut sends no DELETE.
      await db.signOut();
      expect(
        requests.where((r) => r.method == 'DELETE'),
        isEmpty,
        reason: 'a failed register must not be deregistered on signOut',
      );
      await server.close();
    },
  );

  test(
    'unknown platform / empty token throw ArgumentError with no request',
    () async {
      final (server, requests) = await _startServer(204);
      final db = _newDb(server);

      await expectLater(
        db.registerPushToken('gcm', 'tok'),
        throwsArgumentError,
      );
      await expectLater(db.registerPushToken('fcm', ''), throwsArgumentError);
      expect(requests, isEmpty, reason: 'validation must fail before the wire');
      await server.close();
    },
  );

  test(
    'signOut fires deregistration for session-registered tokens (ADR-0037)',
    () async {
      final (server, requests) = await _startServer(204);
      final engine = _PushFakeEngine();
      final db = NostosDatabase.forTest(
        Nostos.withEngine(engine),
        const NostosSchema(tables: []),
        httpBase: 'http://127.0.0.1:${server.port}',
        token: 'jwt-abc',
      );
      await db.registerPushToken('fcm', 'tok-a');
      await db.registerPushToken('webpush', 'tok-b');

      await db.signOut();

      expect(engine.signOutCalls, 1, reason: 'core signOut ran too');
      final deletes = requests.where((r) => r.method == 'DELETE').toList();
      expect(deletes.map((r) => r.path).toSet(), {
        '/push-tokens/tok-a',
        '/push-tokens/tok-b',
      });
      expect(
        deletes.every((r) => r.headers['authorization'] == 'Bearer jwt-abc'),
        isTrue,
      );

      // Idempotent: a second signOut deregisters nothing (set already drained).
      final countBefore = requests.length;
      await db.signOut();
      expect(requests.length, countBefore);

      // L1: the seed token died with the session — a post-signOut REST call
      // carries NO Authorization header (never the previous principal's).
      await db.deregisterPushToken('tok-c');
      final post = requests.last;
      expect(post.method, 'DELETE');
      expect(post.path, '/push-tokens/tok-c');
      expect(post.headers.containsKey('authorization'), isFalse);
      await server.close();
    },
  );

  test(
    'a token with URL-unsafe characters is percent-encoded on DELETE',
    () async {
      final (server, requests) = await _startServer(204);
      final db = _newDb(server);

      await db.deregisterPushToken('tok with spaces/+');

      expect(requests.single.path, '/push-tokens/tok%20with%20spaces%2F%2B');
      await server.close();
    },
  );
}
