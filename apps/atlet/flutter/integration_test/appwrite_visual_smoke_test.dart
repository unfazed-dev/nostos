// Drives the real visual Atlet app against the hosted Appwrite project.
// Credentials enter only through a temporary, ignored dart-define file.

import 'dart:convert';

import 'package:atlet/main.dart' as app;
import 'package:atlet/adapters/nostos_adapter.dart';
import 'package:atlet/cloud_auth.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

const _email = String.fromEnvironment('ATLET_TEST_EMAIL');
const _password = String.fromEnvironment('ATLET_TEST_PASSWORD');
const _role = String.fromEnvironment(
  'ATLET_TEST_ROLE',
  defaultValue: 'customer_a',
);
const _manualConnectivity = bool.fromEnvironment(
  'ATLET_TEST_MANUAL_CONNECTIVITY',
);

void main() {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  binding.defaultTestTimeout = const Timeout(Duration(minutes: 4));
  binding.framePolicy = LiveTestWidgetsFlutterBindingFramePolicy.fullyLive;

  testWidgets('visual offline session reaches Appwrite and returns', (
    tester,
  ) async {
    if (_email.isEmpty || _password.isEmpty) {
      throw StateError('Run through the Rust appwrite_flutter_smoke runner.');
    }
    if (await AtletCloudAuth.instance.session() != null) {
      await AtletCloudAuth.instance.signOut();
    }
    await app.main();
    await tester.pump(const Duration(seconds: 2));
    await tester.enterText(find.byKey(const Key('signin-email')), _email);
    await tester.enterText(find.byKey(const Key('signin-password')), _password);
    await tester.pump(const Duration(milliseconds: 200));
    final signIn = find.widgetWithText(FilledButton, 'Sign in');
    expect(tester.widget<FilledButton>(signIn).onPressed, isNotNull);
    await tester.tap(signIn);
    await tester.pump(const Duration(milliseconds: 200));

    for (var attempt = 0; attempt < 45; attempt++) {
      await tester.pump(const Duration(seconds: 1));
      if (find.byKey(const Key('add-session-button')).evaluate().isNotEmpty) {
        break;
      }
    }
    final visibleText = find
        .byType(Text)
        .evaluate()
        .map((element) => (element.widget as Text).data)
        .whereType<String>()
        .take(25)
        .join(' | ');
    expect(
      find.byKey(const Key('add-session-button')),
      findsOneWidget,
      reason: 'Home never loaded; visible text: $visibleText',
    );
    final adapter = app.engineRegistry.current! as NostosAdapter;
    expect(_manualConnectivity, isTrue);
    expect(adapter.engine, 'nostos-appwrite');
    for (var attempt = 0; attempt < 30 && !adapter.isReady; attempt++) {
      await tester.pump(const Duration(milliseconds: 200));
    }
    expect(adapter.isReady, isTrue);
    expect(adapter.pendingWrites, 0);
    if (_role == 'admin') {
      await _adminCatalog(tester, adapter);
      await _adminUsers(tester, adapter);
      final profileCount = (await adapter.watchUserProfiles().first).length;
      debugPrint(
        'ATLET_EVIDENCE:${jsonEncode({'visible_user_profiles': profileCount, 'pending_writes': await adapter.durablePendingWrites()})}',
      );
      await tester.tap(find.byKey(const Key('nav-tab-home')));
      await tester.pump(const Duration(milliseconds: 500));
      await tester.tap(find.byKey(const Key('sign-out')));
      await _waitForSignIn(tester);
      expect(await AtletCloudAuth.instance.session(), isNull);
      return;
    }
    await adapter.setConnected(false);
    // Confirm the pause left the same local queue empty before creating data.
    expect(await adapter.durablePendingWrites(), 0);
    final title = 'Atlet visual smoke ${DateTime.now().microsecondsSinceEpoch}';
    await tester.tap(find.byKey(const Key('add-session-button')));
    await tester.pump(const Duration(milliseconds: 300));
    await tester.enterText(find.byKey(const Key('session-title-field')), title);
    await tester.enterText(find.byKey(const Key('session-metric-field')), '3');
    await tester.pump(const Duration(milliseconds: 200));
    expect(
      tester
          .widget<FilledButton>(find.byKey(const Key('save-session-button')))
          .onPressed,
      isNotNull,
    );
    await tester.tap(find.byKey(const Key('save-session-button')));
    for (var attempt = 0; attempt < 30; attempt++) {
      await tester.pump(const Duration(milliseconds: 250));
      if (find.byKey(const Key('save-session-button')).evaluate().isEmpty) {
        break;
      }
    }
    expect(find.byKey(const Key('save-session-button')), findsNothing);
    expect(find.text(title), findsOneWidget);
    expect(
      await adapter.durablePendingWrites(),
      greaterThan(0),
      reason:
          'offline write must remain in SQLite (status=${adapter.pendingWrites})',
    );

    final offline = await adapter
        .watchSessions()
        .firstWhere((rows) => rows.any((row) => row.title == title))
        .timeout(const Duration(seconds: 15));
    final created = offline.firstWhere((row) => row.title == title);
    expect(created.serverCommittedAt, isNull);
    await adapter.setConnected(true);
    final committed = await adapter
        .watchSessions()
        .firstWhere(
          (rows) => rows.any(
            (row) => row.id == created.id && row.serverCommittedAt != null,
          ),
        )
        .timeout(const Duration(seconds: 90));
    expect(
      committed.firstWhere((row) => row.id == created.id).serverCommittedAt,
      isNotNull,
    );
    await tester.pump(const Duration(seconds: 1));
    await tester.tap(find.byKey(const Key('nav-tab-shop')));
    await tester.pump(const Duration(seconds: 1));
    expect(find.byKey(const Key('main-nav-bar')), findsOneWidget);

    await adapter.deleteSession(created.id);
    await adapter
        .watchSessions()
        .firstWhere((rows) => rows.every((row) => row.id != created.id))
        .timeout(const Duration(seconds: 30));
    for (
      var attempt = 0;
      attempt < 45 && await adapter.durablePendingWrites() > 0;
      attempt++
    ) {
      await tester.pump(const Duration(seconds: 1));
    }
    expect(
      await adapter.durablePendingWrites(),
      0,
      reason: 'cleanup must reach the cloud before sign-out',
    );
    debugPrint(
      'ATLET_EVIDENCE:${jsonEncode({'converged_sessions': committed.where((row) => row.id == created.id && row.serverCommittedAt != null).length, 'pending_writes': await adapter.durablePendingWrites()})}',
    );
    await tester.tap(find.byKey(const Key('nav-tab-home')));
    await tester.pump(const Duration(milliseconds: 500));
    await tester.tap(find.byKey(const Key('sign-out')));
    await _waitForSignIn(tester);
    expect(await AtletCloudAuth.instance.session(), isNull);
  });
}

Future<void> _waitForSignIn(WidgetTester tester) async {
  for (var attempt = 0; attempt < 60; attempt++) {
    await tester.pump(const Duration(milliseconds: 500));
    if (find.byKey(const Key('signin-email')).evaluate().isNotEmpty) break;
  }
  expect(
    find.byKey(const Key('signin-email')),
    findsOneWidget,
    reason:
        'engine=${app.engineRegistry.activeEngine}; exception=${tester.takeException()}',
  );
}

Future<void> _adminCatalog(WidgetTester tester, NostosAdapter adapter) async {
  await tester.tap(find.byKey(const Key('nav-tab-admin')));
  await tester.pump(const Duration(seconds: 1));
  await tester.tap(find.byKey(const Key('admin-add-product')));
  await tester.pump(const Duration(milliseconds: 300));
  final name =
      'Atlet admin visual smoke ${DateTime.now().microsecondsSinceEpoch}';
  await tester.enterText(find.byKey(const Key('admin-product-name')), name);
  await tester.enterText(
    find.byKey(const Key('admin-product-category')),
    'Equipment',
  );
  await tester.enterText(find.byKey(const Key('admin-product-price')), '15.90');
  await tester.tap(find.byKey(const Key('admin-save-product')));
  await tester.pump(const Duration(seconds: 1));
  expect(find.text(name), findsOneWidget);
  final products = await adapter
      .watchProducts()
      .firstWhere((rows) => rows.any((product) => product.name == name))
      .timeout(const Duration(seconds: 15));
  final created = products.firstWhere((product) => product.name == name);
  for (
    var attempt = 0;
    attempt < 45 && await adapter.durablePendingWrites() > 0;
    attempt++
  ) {
    await tester.pump(const Duration(seconds: 1));
  }
  expect(await adapter.durablePendingWrites(), 0);
  await adapter.deleteProduct(created.id);
  for (
    var attempt = 0;
    attempt < 45 && await adapter.durablePendingWrites() > 0;
    attempt++
  ) {
    await tester.pump(const Duration(seconds: 1));
  }
  expect(
    await adapter.durablePendingWrites(),
    0,
    reason: 'admin catalog cleanup must reach cloud',
  );
}

Future<void> _adminUsers(WidgetTester tester, NostosAdapter adapter) async {
  final profiles = await adapter
      .watchUserProfiles()
      .firstWhere((rows) => rows.any((row) => row.id == 'atlet_user_a_demo'))
      .timeout(const Duration(seconds: 30));
  final customer = profiles.singleWhere((row) => row.id == 'atlet_user_a_demo');
  await tester.tap(find.text('Users'));
  await tester.pump(const Duration(milliseconds: 500));
  expect(find.byKey(const Key('admin-user-list')), findsOneWidget);
  await tester.tap(find.byKey(const Key('admin-edit-user-atlet_user_a_demo')));
  await tester.pump(const Duration(milliseconds: 300));
  final edited = '${customer.displayName} visual';
  await tester.enterText(find.byKey(const Key('admin-user-name')), edited);
  await tester.tap(find.byKey(const Key('admin-save-user')));
  for (var attempt = 0; attempt < 30; attempt++) {
    await tester.pump(const Duration(milliseconds: 250));
    if (find.text(edited).evaluate().isNotEmpty) break;
  }
  expect(find.text(edited), findsOneWidget);
  await adapter.saveUserProfile(customer);
  for (
    var attempt = 0;
    attempt < 45 && await adapter.durablePendingWrites() > 0;
    attempt++
  ) {
    await tester.pump(const Duration(seconds: 1));
  }
  expect(await adapter.durablePendingWrites(), 0);
}
