import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

// | persona | doc (docs/personas/rio.md)              | this journey                                       |
// |---------|-------------------------------------------|----------------------------------------------------|
// | Rio     | custom durations (50/10 blocks) and a     | drives a 4s/2s/5s config (same shape, compressed)  |
// |         | non-default cycle policy (long break      | through 3 work phases to hit the every-3 long-break|
// |         | every 3) hold across a long haul         | policy, asserting counts never inflate.           |
//
// Time is driven by an injected FakeTicker (no wall-clock): each phase is
// advanced by emitting exactly `phaseDuration.inSeconds` ticks, then asserting.

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  // work 4s, short 2s, long 5s, cyclesPerLongBreak 3 — same shape as the
  // persona's 50/10 blocks, compressed for demo time. Same state machine,
  // different numbers; the transition graph depends only on the config.
  const config = TimerConfig(
    work: Duration(seconds: 4),
    shortBreak: Duration(seconds: 2),
    longBreak: Duration(seconds: 5),
    cyclesPerLongBreak: 3,
  );
  const displayKey = Key('timer.display');
  const phaseKey = Key('timer.phase');
  const sessionsKey = Key('timer.sessions');

  testWidgets('Rio: custom durations and a non-default cycle policy hold across '
      'a long run', (tester) async {
    final ticker = FakeTicker();
    addTearDown(ticker.dispose);
    await tester.pumpWidget(PomodoroApp(config: config, ticker: ticker));
    await tester.pump();

    // 1. Open with custom config → display=00:04 (custom work length respected)
    expect(textOf(tester, displayKey), '00:04');
    expect(textOf(tester, sessionsKey), '0');

    // 2. Complete work #1 and its short break → sessions=1, back to Focus
    await tapKey(tester, 'timer.start');
    ticker.tick(config.work.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Short break');
    expect(textOf(tester, sessionsKey), '1');
    await tapKey(tester, 'timer.start');
    ticker.tick(config.shortBreak.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '1');

    // 3. Complete work #2 and its short break → sessions=2, back to Focus
    await tapKey(tester, 'timer.start');
    ticker.tick(config.work.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Short break');
    expect(textOf(tester, sessionsKey), '2');
    await tapKey(tester, 'timer.start');
    ticker.tick(config.shortBreak.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '2');

    // 4. Complete work #3 → phase=Long break (policy: every 3), sessions=3
    await tapKey(tester, 'timer.start');
    ticker.tick(config.work.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Long break');
    expect(textOf(tester, sessionsKey), '3');

    // 5. Complete the long break → phase=Focus, sessions=3
    await tapKey(tester, 'timer.start');
    ticker.tick(config.longBreak.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '3');
  });
}
