import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets(
      'Sam: pause/resume lossless, skip grants nothing, reset is total, '
      'backgrounding freezes',
      (tester) async {
    await tester.pumpWidget(const PomodoroApp(config: TimerConfig.demo()));
    await tester.pump();

    // 1. Start work; pause after ~1s → display frozen, stopped
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.display', '00:02'); // one real tick elapsed
    await tapKey(tester, 'timer.start'); // start toggles to pause (stopped)

    // 2. Wait; confirm display unchanged (no drift while paused). The pump is
    //    only a wait — this is a STATE assertion that the timer is stopped.
    await tester.pump(const Duration(seconds: 2));
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.display')))).data,
        '00:02',
        reason: 'display must not drift while paused');

    // 3. Resume; let work complete → phase=Short break, sessions=1
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Short break');
    await waitForText(tester, 'timer.sessions', '1');

    // 4. Skip the break → phase=Focus, sessions=1 (skip grants nothing)
    await tapKey(tester, 'timer.skip');
    await waitForText(tester, 'timer.phase', 'Focus');
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.sessions')))).data, '1');

    // 5. Start work, then reset mid-phase → phase=Focus, display=00:03,
    //    sessions=0, stopped
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.display', '00:02'); // running, mid-phase
    await tapKey(tester, 'timer.reset');
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.display')))).data,
        '00:03');
    expect((tester.widget<Text>(find.byKey(const Key('timer.phase')))).data,
        'Focus');
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.sessions')))).data, '0');

    // 6. Background the app while running (lifecycle) → auto-paused, frozen.
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.display', '00:02'); // running, ticking
    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.paused);
    await tester.pump();
    final frozen =
        (tester.widget<Text>(find.byKey(const Key('timer.display')))).data;
    // Pump more wall-clock time; a leaking timer would have decremented.
    await tester.pump(const Duration(seconds: 2));
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.display')))).data,
        frozen,
        reason: 'display must be frozen while app is paused');
    // After `resumed`, onLifecycle is a no-op (only non-resumed states pause),
    // so the timer stays stopped until Sam presses start.
    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
    await tester.pump(const Duration(seconds: 2));
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.display')))).data,
        frozen,
        reason: 'after resumed the timer must remain stopped until Sam '
            'presses start');
  });
}
