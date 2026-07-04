import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

// | persona | doc (docs/personas/sam.md)                | this journey                                       |
// |---------|--------------------------------------------|----------------------------------------------------|
// | Sam     | pause/resume lossless; skip grants nothing;| pause then drive ticks (frozen); resume to a work  |
// |         | reset is total; backgrounding freezes the  | completion; skip-no-credit; reset-total; background|
// |         | timer                                      | via lifecycle → auto-paused, frozen through ticks. |
//
// Time is driven by an injected FakeTicker (no wall-clock): ticks are emitted
// explicitly and the display is asserted immediately after a pump. Pausing,
// reset, skip, and lifecycle are STATE assertions — they prove the timer is
// stopped by emitting ticks that a leaking timer would have counted.

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  const config = TimerConfig.demo();
  const displayKey = Key('timer.display');
  const phaseKey = Key('timer.phase');
  const sessionsKey = Key('timer.sessions');

  testWidgets(
      'Sam: pause/resume lossless, skip grants nothing, reset is total, '
      'backgrounding freezes', (tester) async {
    final ticker = FakeTicker();
    addTearDown(ticker.dispose);
    await tester
        .pumpWidget(PomodoroApp(config: config, ticker: ticker));
    await tester.pump();

    // 1. Start work; one tick elapses → display 00:02; pause (start toggles).
    await tapKey(tester, 'timer.start');
    ticker.tick();
    await tester.pump();
    expect(textOf(tester, displayKey), '00:02');
    await tapKey(tester, 'timer.start'); // start toggles to pause (stopped)

    // 2. Emit ticks while paused; display must not drift. This is a STATE
    //    assertion that the timer is stopped (a leaking timer would decrement).
    ticker.tick(5);
    await tester.pump();
    expect(textOf(tester, displayKey), '00:02',
        reason: 'display must not drift while paused');

    // 3. Resume; drive the remaining 2s of work to completion → Short break,
    //    sessions=1 (resume is lossless: the paused second is NOT lost).
    await tapKey(tester, 'timer.start');
    ticker.tick(2);
    await tester.pump();
    expect(textOf(tester, phaseKey), 'Short break');
    expect(textOf(tester, sessionsKey), '1');

    // 4. Skip the break → phase=Focus, sessions=1 (skip grants nothing).
    await tapKey(tester, 'timer.skip');
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '1');

    // 5. Start work, then reset mid-phase → phase=Focus, display=00:03,
    //    sessions=0, stopped.
    await tapKey(tester, 'timer.start');
    ticker.tick(); // running, mid-phase
    await tester.pump();
    expect(textOf(tester, displayKey), '00:02');
    await tapKey(tester, 'timer.reset');
    expect(textOf(tester, displayKey), '00:03');
    expect(textOf(tester, phaseKey), 'Focus');
    expect(textOf(tester, sessionsKey), '0');

    // 6. Background the app while running (lifecycle) → auto-paused, frozen.
    //    This is a lifecycle-observer callback, not a tick — independent of the
    //    ticker mechanism.
    await tapKey(tester, 'timer.start');
    ticker.tick(); // running, ticking
    await tester.pump();
    expect(textOf(tester, displayKey), '00:02');
    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.paused);
    await tester.pump();
    final frozen = textOf(tester, displayKey);
    // Emit ticks while backgrounded; a leaking timer would have decremented.
    ticker.tick(5);
    await tester.pump();
    expect(textOf(tester, displayKey), frozen,
        reason: 'display must be frozen while app is paused');
    // After `resumed`, onLifecycle is a no-op (only non-resumed states pause),
    // so the timer stays stopped until Sam presses start.
    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
    ticker.tick(5);
    await tester.pump();
    expect(textOf(tester, displayKey), frozen,
        reason: 'after resumed the timer must remain stopped until Sam '
            'presses start');
  });
}
