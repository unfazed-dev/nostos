// Tests for the I-1 fix (final-review-verdict.md): the bottom-nav shell in
// main.dart that makes Shop and Analytics reachable from a signed-in Home.
// benchStoreOpener is injected (mirrors AnalyticsScreen's own store
// injection) so the Analytics tab never touches the real path_provider
// platform channel under test.

import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/bench/store.dart';
import 'package:atlet/main.dart';
import 'package:atlet/ui/shop.dart';

/// See test/analytics_test.dart's `_settle` for why `pumpAndSettle()` is
/// wrong here: AnalyticsScreen's indeterminate CircularProgressIndicators
/// (loading state, run/upload buttons) keep scheduling frames forever.
Future<void> _settle(WidgetTester tester) async {
  for (var i = 0; i < 10; i++) {
    await tester.runAsync(
      () => Future<void>.delayed(const Duration(milliseconds: 20)),
    );
    await tester.pump();
  }
}

void main() {
  group('HomeScreen bottom nav', () {
    late Directory tempDir;
    late BenchStore store;

    setUp(() async {
      tempDir = await Directory.systemTemp.createTemp('atlet_nav_test_');
      store = BenchStore(directory: tempDir);
    });

    tearDown(() async {
      if (await tempDir.exists()) await tempDir.delete(recursive: true);
    });

    Widget harness() => MaterialApp(
          home: HomeScreen(benchStoreOpener: () async => store),
        );

    testWidgets('all three tabs are present', (tester) async {
      await tester.pumpWidget(harness());
      await tester.pump();

      expect(find.byKey(const Key('nav-tab-home')), findsOneWidget);
      expect(find.byKey(const Key('nav-tab-shop')), findsOneWidget);
      expect(find.byKey(const Key('nav-tab-analytics')), findsOneWidget);
    });

    testWidgets('starts on Home', (tester) async {
      await tester.pumpWidget(harness());
      await tester.pump();

      expect(find.widgetWithText(AppBar, 'Home'), findsOneWidget);
      expect(find.byType(ShopScreen), findsNothing);
      expect(find.byKey(const Key('analytics-screen')), findsNothing);
    });

    testWidgets('tapping Shop shows ShopScreen', (tester) async {
      await tester.pumpWidget(harness());
      await tester.pump();

      await tester.tap(find.byKey(const Key('nav-tab-shop')));
      await tester.pump();

      // No engine is selected in this harness (engineRegistry.current is
      // null until switchTo() runs, which would hit a real adapter), so
      // ShopScreen renders its "no engine" status branch, not the
      // shop-screen-keyed grid. Proving the tab wiring works only needs
      // ShopScreen to be reached, not loaded with data — shop_test.dart
      // already covers the loaded-with-data branch via a fake adapter.
      expect(find.byType(ShopScreen), findsOneWidget);
    });

    testWidgets('tapping Analytics shows AnalyticsScreen with the eval banner', (
      tester,
    ) async {
      await tester.pumpWidget(harness());
      await tester.pump();

      await tester.tap(find.byKey(const Key('nav-tab-analytics')));
      await _settle(tester);

      expect(find.byKey(const Key('analytics-screen')), findsOneWidget);
      expect(find.byKey(const Key('analytics-eval-banner')), findsOneWidget);
    });

    testWidgets('tapping back to Home returns to the training screen', (
      tester,
    ) async {
      await tester.pumpWidget(harness());
      await tester.pump();

      await tester.tap(find.byKey(const Key('nav-tab-shop')));
      await tester.pump();
      await tester.tap(find.byKey(const Key('nav-tab-home')));
      await tester.pump();

      expect(find.widgetWithText(AppBar, 'Home'), findsOneWidget);
      expect(find.byType(ShopScreen), findsNothing);
    });
  });
}
