import 'dart:async';

import 'package:flutter/widgets.dart' show AppLifecycleState;
import 'package:flutter_test/flutter_test.dart';
import 'package:mocktail/mocktail.dart';
import 'package:pomodoro/domain/ticker.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/viewmodels/pomodoro_viewmodel.dart';

class _MockTicker extends Mock implements Ticker {}

void main() {
  late _MockTicker ticker;
  late StreamController<void> ticks;

  setUp(() {
    ticker = _MockTicker();
    ticks = StreamController<void>.broadcast(sync: true);
    when(() => ticker.ticks()).thenAnswer((_) => ticks.stream);
  });

  tearDown(() => ticks.close());

  PomodoroViewModel vm({TimerConfig config = const TimerConfig.demo()}) =>
      PomodoroViewModel(ticker: ticker, config: config);

  void tick([int n = 1]) {
    for (var i = 0; i < n; i++) {
      ticks.add(null);
    }
  }

  test('initial state: work phase, full duration, stopped, zero sessions', () {
    final m = vm();
    expect(m.phase, Phase.work);
    expect(m.remaining, const TimerConfig.demo().work);
    expect(m.running, isFalse);
    expect(m.completedWork, 0);
  });

  test('start then ticks decrement remaining', () {
    final m = vm()..start();
    tick(2);
    expect(m.remaining, const Duration(seconds: 1));
    expect(m.running, isTrue);
  });

  test('pause freezes remaining; ticks while paused are ignored', () {
    final m = vm()..start();
    tick();
    m.pause();
    tick(5);
    expect(m.remaining, const Duration(seconds: 2));
    expect(m.running, isFalse);
  });

  test('rapid re-entry start-pause-start never double-fires (single subscription)', () {
    final m = vm()..start();
    m.pause();
    m.start();
    m.pause();
    m.start();
    tick(); // exactly one tick must decrement exactly one second
    expect(m.remaining, const Duration(seconds: 2));
    verify(() => ticker.ticks()).called(1); // subscription created once, ever
  });

  test('work completion: session credited, short break queued, stopped (autoAdvance off)', () {
    final m = vm()..start();
    tick(3);
    expect(m.completedWork, 1);
    expect(m.phase, Phase.shortBreak);
    expect(m.remaining, const TimerConfig.demo().shortBreak);
    expect(m.running, isFalse);
  });

  test('long break after cyclesPerLongBreak work phases', () {
    final m = vm()..start();
    tick(3); // work #1 done -> shortBreak
    m.start();
    tick(2); // break done -> work
    m.start();
    tick(3); // work #2 done -> demo config has cyclesPerLongBreak = 2
    expect(m.completedWork, 2);
    expect(m.phase, Phase.longBreak);
    expect(m.remaining, const TimerConfig.demo().longBreak);
  });

  test('break completion returns to work at full duration', () {
    final m = vm()..start();
    tick(3);
    m.start();
    tick(2);
    expect(m.phase, Phase.work);
    expect(m.remaining, const TimerConfig.demo().work);
  });

  test('reset restores initial state from any point', () {
    final m = vm()..start();
    tick(3); // one full work phase
    m.start();
    tick(1); // mid-break
    m.reset();
    expect(m.phase, Phase.work);
    expect(m.remaining, const TimerConfig.demo().work);
    expect(m.running, isFalse);
    expect(m.completedWork, 0);
  });

  test('skip advances the phase WITHOUT crediting a work session', () {
    final m = vm()..start();
    tick(1);
    m.skip();
    expect(m.phase, Phase.shortBreak);
    expect(m.completedWork, 0);
  });

  test('lifecycle: backgrounding auto-pauses a running timer', () {
    final m = vm()..start();
    m.onLifecycle(AppLifecycleState.paused);
    expect(m.running, isFalse);
    tick(5);
    expect(m.remaining, const TimerConfig.demo().work); // frozen
  });

  test('equivalence: transition graph is identical across configs (compressed time is honest)', () {
    // Two configs with different durations but the same cycle policy must
    // produce the same (phase, completedWork) sequence when each phase is
    // driven to completion. This is what licenses demo-config E2E journeys.
    List<(Phase, int)> run(TimerConfig c) {
      final m = vm(config: c);
      final seen = <(Phase, int)>[];
      Duration current() => switch (m.phase) {
            Phase.work => c.work,
            Phase.shortBreak => c.shortBreak,
            Phase.longBreak => c.longBreak,
          };
      for (var i = 0; i < 5; i++) {
        m.start();
        tick(current().inSeconds);
        seen.add((m.phase, m.completedWork));
      }
      return seen;
    }

    final a = run(const TimerConfig(
        work: Duration(seconds: 3),
        shortBreak: Duration(seconds: 2),
        longBreak: Duration(seconds: 4),
        cyclesPerLongBreak: 2));
    final b = run(const TimerConfig(
        work: Duration(seconds: 7),
        shortBreak: Duration(seconds: 5),
        longBreak: Duration(seconds: 9),
        cyclesPerLongBreak: 2));
    expect(a, b);
  });
}
