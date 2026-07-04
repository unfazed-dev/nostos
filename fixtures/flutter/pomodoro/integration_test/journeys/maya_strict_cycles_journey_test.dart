import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

// | persona | doc (docs/personas/maya.md)            | this journey                                     |
// |---------|------------------------------------------|--------------------------------------------------|
// | Maya    | strict pomodoro cycles; long break      | drives 2 work/break cycles through the demo      |
// |         | after every 2 work phases; honest        | config and asserts the long-break policy fires   |
// |         | session count (no credit on break)       | at session 2 and the count never inflates.       |
//
// Time is driven by an injected FakeTicker (no wall-clock): each phase is
// advanced by emitting exactly `phaseDuration.inSeconds` ticks, then asserting.
// This is the Ticker port used the way it was designed — see helpers.dart.

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  const config = TimerConfig.demo();
  const phaseKey = Key('timer.phase');
  const sessionsKey = Key('timer.sessions');

  testWidgets('Maya: strict cycles, correct break policy, honest session count',
      (tester) async {
    final ticker = FakeTicker();
    addTearDown(ticker.dispose);
    await tester
        .pumpWidget(PomodoroApp(config: config, ticker: ticker));
    await tester.pump();

    // 1. Open the app
    expect(find.text('Focus'), findsOneWidget);
    expect(textOf(tester, const Key('timer.display')), '00:03');

    // 2. Start; drive work to completion (3 ticks) → Short break, sessions=1
    await tapKey(tester, 'timer.start');
    ticker.tick(config.work.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Short break');
    expect(textOf(tester, sessionsKey), '1');

    // 3. Start the break; drive it to completion (2 ticks) → Focus, sessions=1
    await tapKey(tester, 'timer.start');
    ticker.tick(config.shortBreak.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '1');

    // 4. Start; 2nd work completion hits the long-break policy (every 2)
    await tapKey(tester, 'timer.start');
    ticker.tick(config.work.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Long break');
    expect(textOf(tester, sessionsKey), '2');

    // 5. Long break completes; back to Focus, count untouched
    await tapKey(tester, 'timer.start');
    ticker.tick(config.longBreak.inSeconds);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '2');
  });
}
