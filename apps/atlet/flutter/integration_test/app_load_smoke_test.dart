// Drives the REAL app (main.dart) on a device: taps the prefilled Sign in,
// reads Home, taps the Shop tab, and reports every 2 s whether a spinner is
// still up and how many rows/cards rendered. Prints APP_SMOKE_* markers.

import 'dart:io' show stderr;

import 'package:atlet/main.dart' as app;
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

// Release builds drop debugPrint on iOS; stderr reaches `devicectl --console`.
void say(String m) => stderr.writeln(m);

Future<void> main() async {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  binding.defaultTestTimeout = const Timeout(Duration(minutes: 6));

  testWidgets('real app load smoke', (tester) async {
    await app.main();
    // Report app-side framework errors instead of failing the drive.
    FlutterError.onError = (d) =>
        say('APP_SMOKE_FLUTTER_ERROR ${d.exceptionAsString()}');
    await tester.pump(const Duration(seconds: 3));
    say('APP_SMOKE_BOOTED signin=${find.text('Sign in').evaluate().length}');
    await tester.tap(find.widgetWithText(FilledButton, 'Sign in'));
    say('APP_SMOKE_TAPPED');
    int count(Finder f) => f.evaluate().length;
    for (var i = 0; i < 5; i++) {
      await tester.pump(const Duration(seconds: 2));
      say('APP_SMOKE_HOME_T${i * 2}s spinners=${count(find.byType(CircularProgressIndicator))} '
          'sessionList=${count(find.byKey(const Key('session-list')))} '
          'empty=${count(find.textContaining('No sessions yet'))}');
    }
    await tester.tap(find.byKey(const Key('nav-tab-shop')));
    say('APP_SMOKE_SHOP_TAPPED');
    for (var i = 0; i < 5; i++) {
      await tester.pump(const Duration(seconds: 2));
      final cards = find.byWidgetPredicate(
        (w) => w.key is ValueKey<String> &&
            (w.key! as ValueKey<String>).value.startsWith('product-card-'),
      );
      say('APP_SMOKE_SHOP_T${i * 2}s spinners=${count(find.byType(CircularProgressIndicator))} '
          'grid=${count(find.byKey(const Key('shop-grid')))} cards=${count(cards)}');
    }
    // Round 2: the sign-out -> sign-in cycle (ADR-0029 wipe, then re-snapshot).
    await tester.tap(find.byKey(const Key('nav-tab-home')));
    await tester.pump(const Duration(seconds: 1));
    await tester.tap(find.byTooltip('Sign out'));
    for (var i = 0; i < 10; i++) {
      await tester.pump(const Duration(seconds: 2));
      final signin = count(find.widgetWithText(FilledButton, 'Sign in'));
      say('APP_SMOKE_SIGNOUT_T${i * 2}s spinners=${count(find.byType(CircularProgressIndicator))} signin=$signin');
      if (signin > 0) break;
    }
    await tester.tap(find.widgetWithText(FilledButton, 'Sign in'));
    say('APP_SMOKE_TAPPED2');
    for (var i = 0; i < 15; i++) {
      await tester.pump(const Duration(seconds: 2));
      say('APP_SMOKE_HOME2_T${i * 2}s spinners=${count(find.byType(CircularProgressIndicator))} '
          'sessionList=${count(find.byKey(const Key('session-list')))} '
          'empty=${count(find.textContaining('No sessions yet'))} '
          'signin=${count(find.widgetWithText(FilledButton, 'Sign in'))}');
    }
    await tester.tap(find.byKey(const Key('nav-tab-shop')));
    for (var i = 0; i < 15; i++) {
      await tester.pump(const Duration(seconds: 2));
      final cards = find.byWidgetPredicate(
        (w) => w.key is ValueKey<String> &&
            (w.key! as ValueKey<String>).value.startsWith('product-card-'),
      );
      say('APP_SMOKE_SHOP2_T${i * 2}s spinners=${count(find.byType(CircularProgressIndicator))} cards=${count(cards)}');
    }
    say('APP_SMOKE_DONE');
  });
}
