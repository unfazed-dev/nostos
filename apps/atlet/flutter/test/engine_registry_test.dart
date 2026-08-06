import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/engine_registry.dart';

/// Records init/signOut calls (with start/end markers so tests can prove
/// ordering, not just that both eventually happened) instead of doing real
/// engine work. [signOutGate], when set, makes signOut() hang until the
/// test explicitly completes it — this is what lets a test observe that
/// switchTo() has NOT yet constructed the incoming adapter while the
/// outgoing one's wipe is still in flight.
class _RecordingAdapter implements SyncAdapter {
  _RecordingAdapter(this.name, this.log, {this.signOutGate});

  final String name;
  final List<String> log;
  final Completer<void>? signOutGate;

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
    if (signOutGate != null) await signOutGate!.future;
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
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );

      final adapter = await registry.start(Engine.cairn, session);

      expect(adapter, isA<_RecordingAdapter>());
      expect(registry.activeEngine, Engine.cairn);
      expect(registry.current, same(adapter));
      expect(registry.debugLiveAdapters, hasLength(1));
      expect(log, ['cairn.init']);
    });

    test('start() throws if an adapter is already live (must go through switchTo)', () async {
      final log = <String>[];
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );
      await registry.start(Engine.cairn, session);

      expect(() => registry.start(Engine.powersync, session), throwsStateError);
    });

    test('switchTo() awaits the outgoing adapter\'s signOut() to completion before constructing/init-ing the incoming one', () async {
      final log = <String>[];
      final gate = Completer<void>();
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log, signOutGate: gate),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );
      await registry.start(Engine.cairn, session);
      log.clear();

      final switchFuture = registry.switchTo(Engine.powersync, session);

      // Let microtasks run; the outgoing signOut() has started but is
      // gated, so the incoming adapter must not have been constructed yet.
      await Future<void>.delayed(Duration.zero);
      expect(log, ['nostos.signOut.start']);
      // Slot is released before the wipe is awaited, not after — nothing is
      // "live" for the whole wipe window (decision #4), not just at the end.
      expect(registry.debugLiveAdapters, isEmpty);

      gate.complete();
      await switchFuture;

      expect(log, ['nostos.signOut.start', 'nostos.signOut.end', 'powersync.init']);
      expect(registry.activeEngine, Engine.powersync);
      expect(registry.debugLiveAdapters, hasLength(1));
    });

    test('only-one-non-null invariant holds after start, switch, and repeated switches', () async {
      final log = <String>[];
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );

      expect(registry.debugLiveAdapters, isEmpty);

      await registry.start(Engine.cairn, session);
      expect(registry.debugLiveAdapters, hasLength(1));

      await registry.switchTo(Engine.powersync, session);
      expect(registry.debugLiveAdapters, hasLength(1));
      expect(registry.debugLiveAdapters.single.engine, 'test');
      expect(registry.activeEngine, Engine.powersync);

      await registry.switchTo(Engine.cairn, session);
      expect(registry.debugLiveAdapters, hasLength(1));
      expect(registry.activeEngine, Engine.cairn);
    });

    test('double switch round-trip returns to the original engine with a fresh instance', () async {
      final log = <String>[];
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );

      final firstNostos = await registry.start(Engine.cairn, session);
      final powersync = await registry.switchTo(Engine.powersync, session);
      final secondNostos = await registry.switchTo(Engine.cairn, session);

      expect(registry.activeEngine, Engine.cairn);
      expect(registry.debugLiveAdapters, hasLength(1));
      expect(identical(firstNostos, secondNostos), isFalse,
          reason: 'switching back must construct a fresh adapter, not reuse the wiped one');
      expect(identical(firstNostos, powersync), isFalse);
      expect(log, [
        'cairn.init',
        'nostos.signOut.start', 'nostos.signOut.end',
        'powersync.init',
        'powersync.signOut.start', 'powersync.signOut.end',
        'cairn.init',
      ]);
    });

    test('switchTo() to the already-active engine is a no-op: no signOut, no new instance', () async {
      final log = <String>[];
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );

      final first = await registry.start(Engine.cairn, session);
      final result = await registry.switchTo(Engine.cairn, session);

      expect(identical(result, first), isTrue);
      expect(log, ['cairn.init']);
      expect(registry.debugLiveAdapters, hasLength(1));
    });

    test('concurrent switchTo() calls are serialized, not raced', () async {
      final log = <String>[];
      final gate = Completer<void>();
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log, signOutGate: gate),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );
      await registry.start(Engine.cairn, session);
      log.clear();

      // Two rapid taps: switch to powersync, then — before that finishes —
      // switch back to nostos. The second call must wait for the first to
      // fully resolve rather than interleaving with it.
      final first = registry.switchTo(Engine.powersync, session);
      final second = registry.switchTo(Engine.cairn, session);

      // First switch is blocked on cairn's signOut(); the second call must
      // not have done anything yet.
      await Future<void>.delayed(Duration.zero);
      expect(log, ['nostos.signOut.start']);

      gate.complete();
      await first;
      await second;

      expect(log, [
        'nostos.signOut.start', 'nostos.signOut.end',
        'powersync.init',
        'powersync.signOut.start', 'powersync.signOut.end',
        'cairn.init',
      ]);
      expect(registry.activeEngine, Engine.cairn);
      expect(registry.debugLiveAdapters, hasLength(1));
    });

    test('switchTo() with nothing live yet just starts the target (no signOut)', () async {
      final log = <String>[];
      final registry = EngineRegistry(
        nostosFactory: () => _RecordingAdapter('cairn', log),
        powerSyncFactory: () => _RecordingAdapter('powersync', log),
      );

      await registry.switchTo(Engine.powersync, session);

      expect(log, ['powersync.init']);
      expect(registry.activeEngine, Engine.powersync);
      expect(registry.debugLiveAdapters, hasLength(1));
    });
  });
}
