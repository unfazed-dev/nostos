import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/engine_registry.dart';

import 'support/fake_cart_orders.dart';

/// Records init/signOut calls (with start/end markers so tests can prove
/// ordering, not just that both eventually happened) instead of doing real
/// engine work.
class _RecordingAdapter with FakeCartOrdersDefaults implements SyncAdapter {
  _RecordingAdapter(this.name, this.log);

  final String name;
  final List<String> log;

  @override
  final String engine = 'test';

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {
    log.add('$name.init');
  }

  @override
  Future<void> signOut() async {
    log.add('$name.signOut.start');
    log.add('$name.signOut.end');
  }

  @override
  Future<String> addSession(SessionRow s) async => s.id;

  @override
  Future<void> deleteSession(String id) async {}

  @override
  Stream<List<SessionRow>> watchSessions() => const Stream.empty();

  @override
  Stream<List<ProductRow>> watchProducts() => const Stream.empty();

  @override
  Stream<bool> get connected => const Stream.empty();

  @override
  Future<void> setConnected(bool up) async {}

  @override
  Stream<SyncMark> get marks => const Stream.empty();
}

void main() {
  const session = SyncSession(
    supabaseUrl: 'http://localhost:3000',
    accessToken: 'test-token',
    userId: 'test-user',
    dbDir: '/tmp/test-db',
  );

  group('EngineRegistry', () {
    test('start() constructs and inits the requested engine; exactly one live adapter', () async {
      final log = <String>[];
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log),
        nostosDirectFactory: () => _RecordingAdapter('direct', log),
      );

      final adapter = await registry.start(Engine.cairn, session);

      expect(adapter, isA<_RecordingAdapter>());
      expect(registry.activeEngine, Engine.cairn);
      expect(registry.current, same(adapter));
      expect(registry.debugLiveAdapters, hasLength(1));
      expect(log, ['cairn.init']);
    });

    test(
      'start() throws if an adapter is already live: one engine per session',
      () async {
        final log = <String>[];
        final registry = EngineRegistry(
          nostosFactory: () => _RecordingAdapter('cairn', log),
          nostosDirectFactory: () => _RecordingAdapter('direct', log),
        );
        await registry.start(Engine.cairn, session);

        expect(
          () => registry.start(Engine.cairnDirect, session),
          throwsStateError,
        );
      },
    );
  });
}
