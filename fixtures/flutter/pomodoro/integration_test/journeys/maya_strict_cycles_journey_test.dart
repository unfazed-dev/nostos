import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('Maya: strict cycles, correct break policy, honest session count',
      (tester) async {
    await tester.pumpWidget(const PomodoroApp(config: TimerConfig.demo()));
    await tester.pump();

    // 1. Open the app
    expect(find.text('Focus'), findsOneWidget);
    expect(find.text('00:03'), findsOneWidget);

    // 2. Start; let work phase complete
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Short break');
    await waitForText(tester, 'timer.sessions', '1');

    // 3. Start the break; let it complete
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');

    // 4. Start; 2nd work completion hits the long-break policy (every 2)
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Long break');
    await waitForText(tester, 'timer.sessions', '2');

    // 5. Long break completes; back to Focus, count untouched
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');
    await waitForText(tester, 'timer.sessions', '2');
  });
}
