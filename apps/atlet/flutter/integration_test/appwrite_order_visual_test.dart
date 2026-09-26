// One real cloud order across the admin and two customers in the same Atlet UI.
// The delivered order remains in the hosted demo project's history.

import 'dart:convert';

import 'package:atlet/adapters/nostos_adapter.dart';
import 'package:atlet/cloud_auth.dart';
import 'package:atlet/main.dart' as app;
import 'package:atlet/push/order_push.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

const _adminEmail = String.fromEnvironment('ATLET_TEST_ADMIN_EMAIL');
const _adminPassword = String.fromEnvironment('ATLET_TEST_ADMIN_PASSWORD');
const _aEmail = String.fromEnvironment('ATLET_TEST_USER_A_EMAIL');
const _aPassword = String.fromEnvironment('ATLET_TEST_USER_A_PASSWORD');
const _bEmail = String.fromEnvironment('ATLET_TEST_USER_B_EMAIL');
const _bPassword = String.fromEnvironment('ATLET_TEST_USER_B_PASSWORD');
const _manualConnectivity = bool.fromEnvironment(
  'ATLET_TEST_MANUAL_CONNECTIVITY',
);

void main() {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  binding.defaultTestTimeout = const Timeout(Duration(minutes: 8));
  binding.framePolicy = LiveTestWidgetsFlutterBindingFramePolicy.fullyLive;

  testWidgets('admin and two customers converge on one Appwrite order', (
    tester,
  ) async {
    expect(_manualConnectivity, isTrue);
    for (final value in [
      _adminEmail,
      _adminPassword,
      _aEmail,
      _aPassword,
      _bEmail,
      _bPassword,
    ]) {
      expect(value, isNotEmpty);
    }
    if (await AtletCloudAuth.instance.session() != null) {
      await AtletCloudAuth.instance.signOut();
    }
    await app.main();
    await _waitFor(tester, find.byKey(const Key('signin-email')));

    final admin = await _login(tester, _adminEmail, _adminPassword);
    final name = 'Atlet order visual ${DateTime.now().microsecondsSinceEpoch}';
    final productId = await _addProduct(tester, admin, name);
    await _signOut(tester);

    final buyer = await _login(tester, _aEmail, _aPassword);
    await tester.tap(find.byKey(const Key('nav-tab-shop')));
    await _waitFor(tester, find.byKey(Key('product-card-$productId')));
    await buyer.setConnected(false);
    await tester.tap(find.byKey(Key('product-card-$productId')));
    await _waitFor(tester, find.byKey(const Key('add-to-cart')));
    await tester.tap(find.byKey(const Key('add-to-cart')));
    await _waitGone(tester, find.byKey(const Key('add-to-cart')));
    await tester.pumpAndSettle();
    await _waitFor(tester, find.byKey(const Key('cart-fab')));
    expect(await buyer.durablePendingWrites(), greaterThan(0));
    await tester.tap(find.byKey(const Key('cart-fab')));
    await _waitFor(tester, find.byKey(const Key('checkout-button')));
    await tester.tap(find.byKey(const Key('checkout-button')));
    await _waitGone(tester, find.byKey(const Key('checkout-button')));
    await tester.pumpAndSettle();
    await _waitFor(tester, find.byKey(const Key('pay-button')));
    await tester.tap(find.byKey(const Key('pay-button')));
    await _waitFor(tester, find.byKey(const Key('order-confirmation')));
    final paid = await buyer.watchOrders().firstWhere(
      (rows) => rows.any(
        (row) =>
            row.status == 'paid' &&
            (row.itemsJson?.contains(productId) ?? false),
      ),
    );
    final order = paid.singleWhere(
      (row) =>
          row.status == 'paid' && (row.itemsJson?.contains(productId) ?? false),
    );
    expect(await buyer.durablePendingWrites(), greaterThan(0));
    await buyer.setConnected(true);
    await _drain(tester, buyer);
    await buyer
        .watchOrderEvents()
        .firstWhere(
          (rows) => rows.any(
            (row) => row.orderId == order.id && row.status == 'paid',
          ),
        )
        .timeout(const Duration(seconds: 90));
    await tester.tap(find.byKey(const Key('order-done')));
    await _waitGone(tester, find.byKey(const Key('order-done')));
    await _signOut(tester);

    final other = await _login(tester, _bEmail, _bPassword);
    await other
        .watchProducts()
        .firstWhere((rows) => rows.any((row) => row.id == productId))
        .timeout(const Duration(seconds: 90));
    expect(
      (await other.watchOrders().first).any((row) => row.id == order.id),
      isFalse,
    );
    expect(
      (await other.watchOrderEvents().first).any(
        (row) => row.orderId == order.id,
      ),
      isFalse,
    );
    await _signOut(tester);

    final fulfiller = await _login(tester, _adminEmail, _adminPassword);
    await fulfiller
        .watchOrders()
        .firstWhere(
          (rows) =>
              rows.any((row) => row.id == order.id && row.status == 'paid'),
        )
        .timeout(const Duration(seconds: 90));
    await tester.tap(find.byKey(const Key('nav-tab-admin')));
    await tester.pump(const Duration(milliseconds: 500));
    await tester.tap(find.text('Orders'));
    await _waitFor(tester, find.byKey(Key('admin-ship-${order.id}')));
    await tester.tap(find.byKey(Key('admin-ship-${order.id}')));
    await fulfiller
        .watchOrders()
        .firstWhere(
          (rows) =>
              rows.any((row) => row.id == order.id && row.status == 'shipped'),
        )
        .timeout(const Duration(seconds: 30));
    await _drain(tester, fulfiller);
    await _expectForegroundBanner(tester, fulfiller, order.id, 'shipped');
    await _waitFor(tester, find.byKey(Key('admin-deliver-${order.id}')));
    await tester.tap(find.byKey(Key('admin-deliver-${order.id}')));
    await fulfiller
        .watchOrders()
        .firstWhere(
          (rows) => rows.any(
            (row) => row.id == order.id && row.status == 'delivered',
          ),
        )
        .timeout(const Duration(seconds: 30));
    await _drain(tester, fulfiller);
    await _expectForegroundBanner(tester, fulfiller, order.id, 'delivered');
    await fulfiller.deleteProduct(productId);
    await _drain(tester, fulfiller);
    await _signOut(tester);

    final returning = await _login(tester, _aEmail, _aPassword);
    await returning
        .watchOrders()
        .firstWhere(
          (rows) => rows.any(
            (row) => row.id == order.id && row.status == 'delivered',
          ),
        )
        .timeout(const Duration(seconds: 90));
    final events = await returning
        .watchOrderEvents()
        .firstWhere(
          (rows) => rows.where((row) => row.orderId == order.id).length == 3,
        )
        .timeout(const Duration(seconds: 90));
    expect(
      events.where((row) => row.orderId == order.id).map((row) => row.status),
      containsAll(['paid', 'shipped', 'delivered']),
    );
    await _signOut(tester);

    final isolated = await _login(tester, _bEmail, _bPassword);
    await isolated.connected
        .firstWhere((connected) => connected)
        .timeout(const Duration(seconds: 90));
    expect(
      (await isolated.watchOrders().first).any((row) => row.id == order.id),
      isFalse,
    );
    expect(
      (await isolated.watchOrderEvents().first).any(
        (row) => row.orderId == order.id,
      ),
      isFalse,
    );
    debugPrint(
      'ATLET_EVIDENCE:${jsonEncode({'order_id': order.id, 'order_events': events.where((row) => row.orderId == order.id).length, 'foreground_banners': events.where((row) => row.orderId == order.id && attemptsFor(pushLog.value, row.id).any((attempt) => attempt.error == null)).length, 'customer_b_isolated': true, 'pending_writes': await isolated.durablePendingWrites()})}',
    );
    await _signOut(tester);
  });
}

Future<void> _expectForegroundBanner(
  WidgetTester tester,
  NostosAdapter adapter,
  String orderId,
  String status,
) async {
  final rows = await adapter.watchOrderEvents().firstWhere(
    (events) => events.any(
      (event) => event.orderId == orderId && event.status == status,
    ),
  );
  final event = rows.singleWhere(
    (row) => row.orderId == orderId && row.status == status,
  );
  for (var attempt = 0; attempt < 40; attempt++) {
    final posts = attemptsFor(pushLog.value, event.id);
    if (posts.isNotEmpty) {
      expect(posts.first.error, isNull);
      expect(posts.first.channel, 'snackbar');
      return;
    }
    await tester.pump(const Duration(milliseconds: 250));
  }
  fail('No foreground banner was posted for $status');
}

Future<NostosAdapter> _login(
  WidgetTester tester,
  String email,
  String password,
) async {
  await _waitFor(tester, find.byKey(const Key('signin-email')));
  await tester.enterText(find.byKey(const Key('signin-email')), email);
  await tester.enterText(find.byKey(const Key('signin-password')), password);
  await tester.pump(const Duration(milliseconds: 200));
  final button = find.widgetWithText(FilledButton, 'Sign in');
  expect(tester.widget<FilledButton>(button).onPressed, isNotNull);
  await tester.tap(button);
  await _waitFor(tester, find.byKey(const Key('add-session-button')));
  final adapter = app.engineRegistry.current! as NostosAdapter;
  for (var attempt = 0; attempt < 60 && !adapter.isReady; attempt++) {
    await tester.pump(const Duration(milliseconds: 250));
  }
  expect(adapter.isReady, isTrue);
  expect(adapter.engine, 'nostos-appwrite');
  return adapter;
}

Future<String> _addProduct(
  WidgetTester tester,
  NostosAdapter adapter,
  String name,
) async {
  await tester.tap(find.byKey(const Key('nav-tab-admin')));
  await _waitFor(tester, find.byKey(const Key('admin-add-product')));
  await tester.tap(find.byKey(const Key('admin-add-product')));
  await _waitFor(tester, find.byKey(const Key('admin-product-name')));
  await tester.enterText(find.byKey(const Key('admin-product-name')), name);
  await tester.enterText(
    find.byKey(const Key('admin-product-category')),
    'Equipment',
  );
  await tester.enterText(find.byKey(const Key('admin-product-price')), '15.90');
  await tester.tap(find.byKey(const Key('admin-save-product')));
  await _waitGone(tester, find.byKey(const Key('admin-save-product')));
  final products = await adapter
      .watchProducts()
      .firstWhere((rows) => rows.any((row) => row.name == name))
      .timeout(const Duration(seconds: 30));
  await _drain(tester, adapter);
  return products.singleWhere((row) => row.name == name).id;
}

Future<void> _signOut(WidgetTester tester) async {
  await tester.tap(find.byKey(const Key('nav-tab-home')));
  await _waitFor(tester, find.byKey(const Key('sign-out')));
  await tester.tap(find.byKey(const Key('sign-out')));
  await _waitFor(tester, find.byKey(const Key('signin-email')));
  expect(app.engineRegistry.current, isNull);
}

Future<void> _drain(WidgetTester tester, NostosAdapter adapter) async {
  for (
    var attempt = 0;
    attempt < 120 && await adapter.durablePendingWrites() > 0;
    attempt++
  ) {
    await tester.pump(const Duration(milliseconds: 500));
  }
  expect(await adapter.durablePendingWrites(), 0);
  expect(
    adapter.syncStatus.deadLetteredWrites,
    0,
    reason: adapter.syncStatus.lastWriteError,
  );
}

Future<void> _waitFor(WidgetTester tester, Finder finder) async {
  for (var attempt = 0; attempt < 120; attempt++) {
    await tester.pump(const Duration(milliseconds: 500));
    if (finder.evaluate().isNotEmpty) return;
  }
  expect(finder, findsWidgets);
}

Future<void> _waitGone(WidgetTester tester, Finder finder) async {
  for (var attempt = 0; attempt < 60; attempt++) {
    await tester.pump(const Duration(milliseconds: 250));
    if (finder.evaluate().isEmpty) return;
  }
  expect(finder, findsNothing);
}
