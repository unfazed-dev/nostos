import 'adapters/nostos_adapter.dart';
import 'adapters/powersync_adapter.dart';
import 'adapters/sync_adapter.dart';

enum Engine { nostos, powersync }

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

/// Owns which sync engine is live and enforces plan decision #4: nostos and
/// PowerSync must never be live at the same time. Every engine switch runs a
/// full wipe ([SyncAdapter.signOut]) on the outgoing adapter — and that
/// signOut() completes — before the incoming adapter is constructed and
/// init()'d.
///
/// Two separate nullable slots (rather than one `SyncAdapter? current`)
/// make the "only one live" invariant checkable: [_assertInvariant] asserts
/// they're never both non-null, and every mutation is routed through
/// [_setSlot]/[_clearSlots] so that invariant holds after every call.
/// Adapter construction goes through injectable factories so tests can swap
/// in fakes without touching the real Nostos/PowerSync SDKs.
class EngineRegistry {
  EngineRegistry({
    SyncAdapter Function()? nostosFactory,
    SyncAdapter Function()? powerSyncFactory,
  })  : _nostosFactory = nostosFactory ?? (() => NostosAdapter()),
        _powerSyncFactory = powerSyncFactory ?? (() => PowerSyncAdapter());

  final SyncAdapter Function() _nostosFactory;
  final SyncAdapter Function() _powerSyncFactory;

  SyncAdapter? _nostosAdapter;
  SyncAdapter? _powerSyncAdapter;
  Engine? _activeEngine;

  // Serializes switchTo() calls: two rapid taps on the settings sheet must
  // not interleave (the second seeing stale _activeEngine mid-wipe and
  // double-signing-out or racing start()). Each call waits for any in-flight
  // switch to finish before running its own.
  Future<SyncAdapter>? _switchInFlight;

  Engine? get activeEngine => _activeEngine;

  SyncAdapter? get current => switch (_activeEngine) {
        Engine.cairn => _nostosAdapter,
        Engine.powersync => _powerSyncAdapter,
        null => null,
      };

  /// Debug/test hook: the adapters currently held live. Should always have
  /// length 0 or 1 — see [_assertInvariant], which is the enforcement point;
  /// this getter just makes that invariant observable from tests.
  List<SyncAdapter> get debugLiveAdapters => [?_nostosAdapter, ?_powerSyncAdapter];

  /// Brings up [engine] cold: no prior adapter is torn down first. Throws
  /// [StateError] if an adapter is already live — callers that may already
  /// have one running should use [switchTo] instead, which wipes it first.
  Future<SyncAdapter> start(Engine engine, SyncSession session) async {
    if (_activeEngine != null) {
      throw StateError(
        'EngineRegistry.start() called while ${_activeEngine!.name} is '
        'already live — call switchTo() to wipe and swap instead',
      );
    }
    final adapter =
        engine == Engine.cairn ? _nostosFactory() : _powerSyncFactory();
    _setSlot(engine, adapter);
    await adapter.init(
      supabaseUrl: session.supabaseUrl,
      accessToken: session.accessToken,
      userId: session.userId,
      dbDir: session.dbDir,
    );
    return adapter;
  }

  /// Switches to [target]. If a different engine is currently live, its
  /// adapter is fully wiped via [SyncAdapter.signOut] — awaited to
  /// completion — before [target]'s adapter is constructed and init()'d, so
  /// the two are never live at once (decision #4). No-op (returns the
  /// current adapter) if [target] is already active. Concurrent calls are
  /// serialized — a call made while another is still in flight waits for it
  /// rather than racing it.
  Future<SyncAdapter> switchTo(Engine target, SyncSession session) async {
    while (_switchInFlight != null) {
      try {
        await _switchInFlight;
      } catch (_) {
        // Another caller's switch failed — irrelevant to ours; proceed to
        // attempt our own now that it's no longer in flight.
      }
    }
    final future = _switchToLocked(target, session);
    _switchInFlight = future;
    try {
      return await future;
    } finally {
      if (identical(_switchInFlight, future)) {
        _switchInFlight = null;
      }
    }
  }

  Future<SyncAdapter> _switchToLocked(Engine target, SyncSession session) async {
    if (_activeEngine == target) {
      return current!;
    }
    final outgoing = current;
    if (outgoing != null) {
      // Relinquish the slot before awaiting the wipe, not after: the "only
      // one live" invariant should hold for the whole wipe window, not just
      // the instant after it — once signOut() is called, this adapter is
      // being torn down, not the active engine.
      _clearSlots();
      await outgoing.signOut(); // full wipe — must complete before swap
    }
    _assertInvariant();
    return start(target, session);
  }

  void _setSlot(Engine engine, SyncAdapter adapter) {
    switch (engine) {
      case Engine.cairn:
        _nostosAdapter = adapter;
      case Engine.powersync:
        _powerSyncAdapter = adapter;
    }
    _activeEngine = engine;
    _assertInvariant();
  }

  void _clearSlots() {
    _nostosAdapter = null;
    _powerSyncAdapter = null;
    _activeEngine = null;
  }

  void _assertInvariant() {
    assert(
      !(_nostosAdapter != null && _powerSyncAdapter != null),
      'EngineRegistry: nostos and powersync adapters must never both be '
      'live at once (decision #4)',
    );
  }
}
