import 'dart:async';
import 'dart:io';

import 'package:connectivity_plus/connectivity_plus.dart';

/// Bridges platform connectivity events to the active sync engine so the app
/// reacts to network loss/regain *immediately* instead of waiting for the
/// engine's idle backstop (30s, `IDLE_RECONNECT_BACKSTOP` in
/// sdk/nostos_flutter/rust/src/api/nostos.rs) to notice a dead socket.
///
/// - Loss (only [ConnectivityResult.none] reported): `onOnlineChanged(false)`
///   → adapter disconnects cleanly, the `connected` stream flips offline in
///   the UI right away, and queued writes stay in the outbox.
/// - Regain: `onOnlineChanged(true)` → adapter `resume()` short-circuits the
///   reconnect backoff, replays the outbox, and re-subscribes.
///
/// ## Active probe fallback (simulator support)
///
/// On the iOS *simulator* NWPathMonitor tracks the **macOS** network stack and
/// often never fires (or reports `.unsatisfied` forever) when the Mac's Wi-Fi
/// toggles — a known connectivity_plus/Apple limitation. So alongside the
/// event stream, the guard runs a cheap active probe (TCP connect to
/// 1.1.1.1:443, 3s timeout, every 5s) and feeds the result through the same
/// deduped path. Whichever signal notices a change first wins; identical
/// states are ignored. The probe only runs in real-platform usage (it is
/// skipped when a test injects a fake event stream without a fake probe).
///
/// Pure wiring: the event stream and the probe are injectable so tests can
/// drive them without platform channels — see connectivity_guard_test.dart.
class ConnectivityGuard {
  ConnectivityGuard({
    required this._onOnlineChanged,
    this._events,
    this._probe,
    this._probeInterval = const Duration(seconds: 5),
  });

  final Future<void> Function(bool online) _onOnlineChanged;
  final Stream<List<ConnectivityResult>>? _events;
  final Future<bool> Function()? _probe;
  final Duration _probeInterval;
  StreamSubscription<List<ConnectivityResult>>? _sub;
  Timer? _probeTimer;
  bool _probing = false;

  /// connectivity_plus re-reports identical states (e.g. on app foreground);
  /// dedupe so we don't disconnect/resume a healthy session redundantly.
  /// Seeded `true`: the app boots assuming online, so the initial probe/event
  /// confirming "online" is a no-op — only real *changes* fire the callback.
  /// (An initial resume() while the engine is still subscribing crashes with
  /// "watch() called before subscribe()".)
  bool? _lastOnline = true;

  void start() {
    _sub ??=
        (_events ?? Connectivity().onConnectivityChanged).listen((results) {
      final online = results.any((r) => r != ConnectivityResult.none);
      _apply(online);
    });
    // Real usage (no injected stream) or a test that injects a probe: poll.
    // Widget tests that inject only a fake event stream get no real sockets.
    if (_events == null || _probe != null) {
      _probeTimer ??=
          Timer.periodic(_probeInterval, (_) => unawaited(_runProbe()));
    }
  }

  Future<void> _runProbe() async {
    if (_probing) return; // don't stack slow probes
    _probing = true;
    try {
      final online = await (_probe ?? _defaultProbe)();
      _apply(online);
    } finally {
      _probing = false;
    }
  }

  /// Ground-truth reachability: a real TCP handshake to a well-known anycast
  /// IP (no DNS, so no resolver caching can fake an "online" answer).
  static Future<bool> _defaultProbe() async {
    try {
      final socket = await Socket.connect(
        '1.1.1.1',
        443,
        timeout: const Duration(seconds: 3),
      );
      socket.destroy();
      return true;
    } on Object {
      return false;
    }
  }

  void _apply(bool online) {
    if (online == _lastOnline) return;
    _lastOnline = online;
    unawaited(_onOnlineChanged(online));
  }

  Future<void> dispose() async {
    _probeTimer?.cancel();
    _probeTimer = null;
    await _sub?.cancel();
    _sub = null;
  }
}
