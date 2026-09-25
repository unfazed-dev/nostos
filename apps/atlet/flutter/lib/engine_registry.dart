import 'adapters/nostos_adapter.dart';
import 'adapters/sync_adapter.dart';

/// Which Nostos client is live. `nostosDirect` is the same engine with no
/// `nostos-server` on the other end — the device syncs with Supabase itself
/// (ADR-0045) — so it is a second engine here, not a flag on [Engine.nostos]:
/// the two hold different databases and must never be live at once.
enum Engine { nostos, nostosDirect }

/// Session/env parameters needed to bring an adapter up from cold — mirrors
/// [SyncAdapter.init]'s named parameters as one value so callers don't have
/// to thread four strings through [EngineRegistry].
class SyncSession {
  const SyncSession({
    required this.supabaseUrl,
    required this.accessToken,
    required this.userId,
    required this.dbDir,
  });

  final String supabaseUrl;
  final String accessToken;
  final String userId;
  final String dbDir;
}

/// Owns which engine is live and enforces plan decision #4: two engines must
/// never be live at the same time, because they hold two databases. The app
/// picks its engine once, at startup, and cannot change it (user request
/// 2026-09-22), so this is [start] and nothing else — decision #4 now holds
/// because there is no swap to get wrong, and [start] throws rather than
/// bringing a second adapter up beside a live one.
///
/// Separate nullable slots per engine (rather than one `SyncAdapter? current`)
/// keep the "only one live" invariant checkable: [_assertInvariant] asserts
/// they're never both non-null, and the one mutation point is [_setSlot].
/// Adapter construction goes through injectable factories so tests can swap
/// in fakes without touching the real Nostos SDK.
class EngineRegistry {
  EngineRegistry({
    SyncAdapter Function()? nostosFactory,
    SyncAdapter Function()? nostosDirectFactory,
    String supabaseAnonKey = '',
  }) : _nostosFactory = nostosFactory ?? (() => NostosAdapter()),
       _nostosDirectFactory =
           nostosDirectFactory ??
           (() => NostosAdapter.direct(anonKey: supabaseAnonKey));

  final SyncAdapter Function() _nostosFactory;
  final SyncAdapter Function() _nostosDirectFactory;

  SyncAdapter? _nostosAdapter;
  SyncAdapter? _nostosDirectAdapter;
  Engine? _activeEngine;

  Engine? get activeEngine => _activeEngine;

  SyncAdapter? get current => switch (_activeEngine) {
    Engine.nostos => _nostosAdapter,
    Engine.nostosDirect => _nostosDirectAdapter,
    null => null,
  };

  /// Debug/test hook: the adapters currently held live. Should always have
  /// length 0 or 1 — see [_assertInvariant], which is the enforcement point;
  /// this getter just makes that invariant observable from tests.
  List<SyncAdapter> get debugLiveAdapters => [
    ?_nostosAdapter,
    ?_nostosDirectAdapter,
  ];

  /// Brings up [engine] cold. Throws [StateError] if an adapter is already
  /// live: the app starts one engine for the session, and two adapters
  /// holding two databases at once is exactly what decision #4 forbids.
  Future<SyncAdapter> start(Engine engine, SyncSession session) async {
    if (_activeEngine != null) {
      throw StateError(
        'EngineRegistry.start() called while ${_activeEngine!.name} is '
        'already live — one engine per session, no swapping',
      );
    }
    final adapter = switch (engine) {
      Engine.nostos => _nostosFactory(),
      Engine.nostosDirect => _nostosDirectFactory(),
    };
    _setSlot(engine, adapter);
    await adapter.init(
      supabaseUrl: session.supabaseUrl,
      accessToken: session.accessToken,
      userId: session.userId,
      dbDir: session.dbDir,
    );
    return adapter;
  }

  /// Tears the live engine down: `signOut()` on the adapter (disconnect +
  /// local wipe, ADR-0029) and an empty slot, so the next sign-in can
  /// [start] cold. No-op when nothing is live.
  Future<void> stop() async {
    final adapter = current;
    _activeEngine = null;
    _nostosAdapter = null;
    _nostosDirectAdapter = null;
    await adapter?.signOut();
  }

  void _setSlot(Engine engine, SyncAdapter adapter) {
    switch (engine) {
      case Engine.nostos:
        _nostosAdapter = adapter;
      case Engine.nostosDirect:
        _nostosDirectAdapter = adapter;
    }
    _activeEngine = engine;
    _assertInvariant();
  }

  void _assertInvariant() {
    assert(
      debugLiveAdapters.length <= 1,
      'EngineRegistry: at most one adapter may be live at once '
      '(decision #4) — found ${debugLiveAdapters.length}',
    );
  }
}
