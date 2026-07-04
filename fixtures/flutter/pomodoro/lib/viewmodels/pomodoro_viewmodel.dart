import 'dart:async';

import 'package:flutter/widgets.dart';

import '../domain/ticker.dart';
import '../domain/timer_config.dart';

enum Phase { work, shortBreak, longBreak }

/// The pomodoro state machine. All timing flows through the [Ticker] port;
/// the machine itself never touches a real clock.
class PomodoroViewModel extends ChangeNotifier {
  PomodoroViewModel({required this._ticker, this.config = const TimerConfig()})
      : _remaining = config.work;

  final TimerConfig config;
  final Ticker _ticker;
  StreamSubscription<void>? _sub;

  Phase _phase = Phase.work;
  Duration _remaining;
  bool _running = false;
  int _completedWork = 0;

  Phase get phase => _phase;
  Duration get remaining => _remaining;
  bool get running => _running;
  int get completedWork => _completedWork;

  void start() {
    if (_running) return;
    _running = true;
    // Single subscription for the VM's lifetime: double-start cannot
    // double-fire; pause gates ticks instead of tearing down the stream.
    _sub ??= _ticker.ticks().listen((_) => _onTick());
    notifyListeners();
  }

  void pause() {
    if (!_running) return;
    _running = false;
    notifyListeners();
  }

  void reset() {
    _running = false;
    _phase = Phase.work;
    _completedWork = 0;
    _remaining = config.work;
    notifyListeners();
  }

  /// Advance to the next phase without crediting a completed work session.
  void skip() {
    _transition(credit: false);
  }

  /// Auto-pause when the app leaves the foreground.
  // ponytail: pause-on-background; wall-clock catch-up when mobile
  // background continuation matters (the future nostos-sdk retrofit).
  void onLifecycle(AppLifecycleState state) {
    if (state != AppLifecycleState.resumed) pause();
  }

  void _onTick() {
    if (!_running) return;
    _remaining -= const Duration(seconds: 1);
    if (_remaining <= Duration.zero) {
      _transition(credit: _phase == Phase.work);
    } else {
      notifyListeners();
    }
  }

  void _transition({required bool credit}) {
    if (_phase == Phase.work) {
      if (credit) _completedWork++;
      _phase = (_completedWork > 0 && _completedWork % config.cyclesPerLongBreak == 0)
          ? Phase.longBreak
          : Phase.shortBreak;
    } else {
      _phase = Phase.work;
    }
    _remaining = switch (_phase) {
      Phase.work => config.work,
      Phase.shortBreak => config.shortBreak,
      Phase.longBreak => config.longBreak,
    };
    _running = config.autoAdvance;
    notifyListeners();
  }

  @override
  void dispose() {
    _sub?.cancel();
    super.dispose();
  }
}
