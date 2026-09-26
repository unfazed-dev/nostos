// A fresh Appwrite device must explain an inactive account and offer sign-out.
// The Rust runner disables and restores the shared customer B profile.

import 'dart:convert';

import 'package:atlet/adapters/nostos_adapter.dart';
import 'package:atlet/cloud_auth.dart';
import 'package:atlet/main.dart' as app;
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

const _email = String.fromEnvironment('ATLET_TEST_EMAIL');
const _password = String.fromEnvironment('ATLET_TEST_PASSWORD');
const _role = String.fromEnvironment('ATLET_TEST_ROLE');

void main() {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  binding.defaultTestTimeout = const Timeout(Duration(minutes: 4));
  binding.framePolicy = LiveTestWidgetsFlutterBindingFramePolicy.fullyLive;

  testWidgets('inactive account is visible before first sync', (tester) async {
    expect(_role, 'customer_b');
    expect(_email, isNotEmpty);
    expect(_password, isNotEmpty);
    if (await AtletCloudAuth.instance.session() != null) {
      await AtletCloudAuth.instance.signOut();
    }
    await app.main();
    await _waitFor(tester, find.byKey(const Key('signin-email')));
    await tester.enterText(find.byKey(const Key('signin-email')), _email);
    await tester.enterText(find.byKey(const Key('signin-password')), _password);
    await tester.pump(const Duration(milliseconds: 200));
    await tester.tap(find.widgetWithText(FilledButton, 'Sign in'));
    await _waitFor(tester, find.byKey(const Key('engine-start-error')));
    expect(
      find.text('Account access revoked. Sign out to continue.'),
      findsOneWidget,
    );
    expect(find.byKey(const Key('nav-tab-admin')), findsNothing);
    final adapter = app.engineRegistry.current! as NostosAdapter;
    expect(await adapter.durablePendingWrites(), 0);
    debugPrint(
      'ATLET_EVIDENCE:${jsonEncode({'inactive_before_first_sync': true, 'revoked_banner': true, 'admin_tab_visible': false, 'pending_writes': await adapter.durablePendingWrites()})}',
    );
    await tester.tap(find.widgetWithText(TextButton, 'Sign out'));
    await _waitFor(tester, find.byKey(const Key('signin-email')));
    expect(await AtletCloudAuth.instance.session(), isNull);
  });
}

Future<void> _waitFor(WidgetTester tester, Finder finder) async {
  for (var attempt = 0; attempt < 90; attempt++) {
    await tester.pump(const Duration(seconds: 1));
    if (finder.evaluate().isNotEmpty) return;
  }
  fail('Timed out waiting for $finder');
}
