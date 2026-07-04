import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

/// Polls the live widget tree until [key]'s Text widget shows [text].
/// Journeys assert state transitions (phase labels, session counts) and use
/// this to WAIT — they never assert elapsed wall-clock time (flaky under load).
Future<void> waitForText(
  WidgetTester tester,
  String key,
  String text, {
  Duration timeout = const Duration(seconds: 15),
}) async {
  final deadline = DateTime.now().add(timeout);
  while (DateTime.now().isBefore(deadline)) {
    await tester.pump(const Duration(milliseconds: 100));
    final finder = find.byKey(Key(key));
    if (finder.evaluate().isNotEmpty &&
        (tester.widget<Text>(finder)).data == text) {
      return;
    }
  }
  fail('timed out waiting for $key == "$text"');
}

Future<void> tapKey(WidgetTester tester, String key) async {
  await tester.tap(find.byKey(Key(key)));
  await tester.pump();
}
