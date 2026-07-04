import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('Rio: custom durations and a non-default cycle policy hold across '
      'a long run', (tester) async {
    // config: work 4s, short 2s, long 5s, cyclesPerLongBreak 3 — same shape as
    // the persona's 50/10 blocks, compressed for demo time. Same state machine,
    // different numbers; the transition graph depends only on the config.
    const config = TimerConfig(
      work: Duration(seconds: 4),
      shortBreak: Duration(seconds: 2),
      longBreak: Duration(seconds: 5),
      cyclesPerLongBreak: 3,
    );
    await tester.pumpWidget(const PomodoroApp(config: config));
    await tester.pump();

    // 1. Open with custom config → display=00:04 (custom work length respected)
    expect((tester.widget<Text>(find.byKey(const Key('timer.display')))).data,
        '00:04');
    expect((tester.widget<Text>(find.byKey(const Key('timer.sessions')))).data,
        '0');

    // 2. Complete work #1 and its short break → sessions=1, back to Focus
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Short break');
    await waitForText(tester, 'timer.sessions', '1');
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.sessions')))).data, '1');

    // 3. Complete work #2 and its short break → sessions=2, back to Focus
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Short break');
    await waitForText(tester, 'timer.sessions', '2');
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.sessions')))).data, '2');

    // 4. Complete work #3 → phase=Long break (policy: every 3), sessions=3
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Long break');
    await waitForText(tester, 'timer.sessions', '3');

    // 5. Complete the long break → phase=Focus, sessions=3
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');
    expect(
        (tester.widget<Text>(find.byKey(const Key('timer.sessions')))).data, '3');
  });
}
