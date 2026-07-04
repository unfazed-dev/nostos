import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/main.dart' as app;

/// Smoke layer (tester-skill ladder): the real entrypoint boots, the first
/// screen renders, no crash. Nothing more — journeys own behavior.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('app boots and renders the timer screen', (tester) async {
    app.main();
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('timer.display')), findsOneWidget);
    expect(find.byKey(const Key('timer.start')), findsOneWidget);
    expect(find.byKey(const Key('timer.phase')), findsOneWidget);
  });
}
