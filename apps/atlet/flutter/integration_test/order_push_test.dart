// PILOT (ADR-0037) — ecommerce order-lifecycle push smoke, driven by
// tool/push_smoke.sh (leg 2). The reference model for other SDKs: the REAL
// atlet app buys something; the script plays the vendor advancing the order
// paid → shipped → delivered; each commit sends a VISIBLE notification
// (NOSTOS_PUSH_TABLES `orders:visible:<title>:<body>`).
//
//   app (this test, foregrounded) ── Shop → cart → checkout → Pay ──▶
//     nostos-server write-back ──▶ docker PG `orders` row (status=paid)
//   app pauseSync (offline — pushes only target offline accounts)
//   script: UPDATE orders SET status='shipped' ─▶ replication ─▶
//     visible push ─▶ FCM ─▶ this test's onMessage (notification+data)
//   …then 'delivered' — same dance.
//
// Foregrounded notification messages arrive via onMessage without a tray
// notification (FlutterFire documented behavior); backgrounded, the same
// messages land in the system tray — that's the manual demo of this leg.
//
// Dart-defines: same as push_smoke_test.dart, plus ATLET_PUSH_PILOT=true so the
// app's push pilot (lib/push/push_pilot.dart) registers the FCM token.

import 'dart:async';

import 'package:atlet/adapters/nostos_adapter.dart';
import 'package:atlet/main.dart' as app;
import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

const _supabaseUrl = String.fromEnvironment('SUPABASE_URL');
const _supabaseAnonKey = String.fromEnvironment('SUPABASE_ANON_KEY');

/// Poll `f` until it returns true, one pump+second at a time. Frames keep
/// flowing while we wait for replication (products, order rows) — a bare
/// `pumpAndSettle` would spin on the shop's streams and time out.
Future<bool> _until(
  WidgetTester tester,
  bool Function() f, {
  int budgetSeconds = 90,
}) async {
  for (var i = 0; i < budgetSeconds; i++) {
    if (f()) return true;
    await tester.pump(const Duration(seconds: 1));
  }
  return f();
}

Future<void> main() async {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  binding.defaultTestTimeout = const Timeout(Duration(minutes: 10));

  testWidgets('ecommerce order lifecycle pushes (shipped, delivered)', (
    tester,
  ) async {
    if (_supabaseUrl.isEmpty || _supabaseAnonKey.isEmpty) {
      throw StateError(
        'SUPABASE_URL / SUPABASE_ANON_KEY dart-defines required — '
        'run via apps/atlet/flutter/tool/push_smoke.sh',
      );
    }

    await Firebase.initializeApp();
    await Supabase.initialize(
      url: _supabaseUrl,
      publishableKey: _supabaseAnonKey,
    );
    await FirebaseMessaging.instance.requestPermission();

    // ---- real UI: sign in (prefilled) → home → engine autostarts ---------
    await tester.pumpWidget(const app.AtletApp());
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Sign in'));
    // Sign-in is a live network round-trip; pumpAndSettle would return while
    // the request is still in flight. Poll for the home shell instead.
    final signedIn = await _until(
      tester,
      () => tester.any(find.byKey(const Key('home-shell'))),
      budgetSeconds: 60,
    );
    if (!signedIn) throw StateError('UI sign-in never reached home');

    // Engine autostart is post-frame; wait for the live adapter.
    final engineUp = await _until(
      tester,
      () => app.engineRegistry.current is NostosAdapter,
      budgetSeconds: 60,
    );
    if (!engineUp) throw StateError('nostos engine never autostarted');

    // Register THIS run's FCM token explicitly. The app's own pilot attach
    // (push_pilot.dart) also registers, but its getToken() can lag a fresh
    // install indefinitely — attempt-13 post-mortem: no token row ever
    // landed, the router's flush pruned the previous run's dead token and
    // had nothing left to send to (silent empty-tokens path). The smoke
    // owns its preconditions.
    final fcmToken = await FirebaseMessaging.instance.getToken().timeout(
      const Duration(seconds: 60),
    );
    if (fcmToken == null || fcmToken.isEmpty) {
      throw StateError('no FCM token on the order leg');
    }
    debugPrint('PUSH_SMOKE_ORDER_TOKEN=${fcmToken.substring(0, 12)}…');
    await (app.engineRegistry.current as NostosAdapter).registerPushToken(
      'fcm',
      fcmToken,
    );

    // ---- real UI: Shop → first product → Add to cart → cart → checkout ---
    await tester.tap(find.byKey(const Key('nav-tab-shop')));
    await tester.pumpAndSettle();
    final productCard = find.byWidgetPredicate(
      (w) => w.key != null && w.key.toString().contains('product-card-'),
    );
    final shopped = await _until(tester, () => tester.any(productCard));
    if (!shopped) throw StateError('no products replicated into the shop');
    await tester.tap(productCard.first);
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('add-to-cart')));
    // Add-to-cart awaits a cart snapshot + a write before popping the sheet;
    // pumpAndSettle returns during that gap and the sheet still covers the
    // FAB. Poll for the sheet's actual close (and each next sheet's open).
    final sheetClosed = await _until(
      tester,
      () => !tester.any(find.byKey(const Key('product-detail'))),
      budgetSeconds: 30,
    );
    if (!sheetClosed) {
      throw StateError('product sheet never closed after add-to-cart');
    }
    // The "Added …" SnackBar overlaps the cart FAB and eats its taps for
    // its ~4s display — let it dismiss first.
    await _until(
      tester,
      () => !tester.any(find.byType(SnackBar)),
      budgetSeconds: 15,
    );
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('cart-fab')));
    final cartOpen = await _until(
      tester,
      () => tester.any(find.byKey(const Key('cart-sheet'))),
      budgetSeconds: 30,
    );
    if (!cartOpen) throw StateError('cart sheet never opened');
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('checkout-button')));
    final checkoutOpen = await _until(
      tester,
      () => tester.any(find.byKey(const Key('checkout-sheet'))),
      budgetSeconds: 30,
    );
    if (!checkoutOpen) throw StateError('checkout sheet never opened');
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('pay-button')));
    // Mock payment delay is 900ms; then the order write hits the outbox.
    await tester.pump(const Duration(seconds: 2));
    final confirmed = await _until(
      tester,
      () => tester.any(find.byKey(const Key('order-confirmation'))),
      budgetSeconds: 30,
    );
    if (!confirmed) throw StateError('checkout never confirmed the order');

    // ---- the order id the app just wrote (latest demo payment) ----------
    final adapter = app.engineRegistry.current as NostosAdapter;
    String? orderId;
    final gotOrder = await _until(tester, () {
      // watch() replays the latest snapshot; the local outbox row is in it.
      final orders = adapter.watchOrders();
      orders.first.then((rows) {
        for (final o in rows) {
          if (o.paymentRef == 'demo-visa-4242') {
            orderId = o.id;
            break;
          }
        }
      });
      return orderId != null;
    }, budgetSeconds: 30);
    if (!gotOrder || orderId == null) {
      throw StateError('order never appeared in watchOrders()');
    }
    debugPrint('PUSH_SMOKE_ORDER=$orderId');

    // ---- listen for the vendor's lifecycle pushes ------------------------
    // Visible pushes carry notification title/body (template: "Your order
    // {id} is {status}") + the {table, lsn} data. The app's own push pilot
    // resumes the engine on every doorbell (lib/push/push_pilot.dart); push
    // sends re-check presence at flush time, so re-pause after each arrival
    // to stay eligible for the NEXT lifecycle push.
    final shipped = Completer<void>();
    final delivered = Completer<void>();
    final sub = FirebaseMessaging.onMessage.listen((message) {
      final n = message.notification;
      // Silent doorbells (data-only {table,lsn}) are leg 1's concern; this
      // leg's pushes are VISIBLE — `notification` title/body, no data block
      // (the rail only attaches data to silent payloads). Match on body.
      if (n == null) return;
      debugPrint('PUSH_SMOKE_ORDER_MESSAGE title=${n.title} body=${n.body}');
      final body = n.body ?? '';
      if (body.contains('shipped') && !shipped.isCompleted) {
        debugPrint('PUSH_SMOKE_PUSH=shipped');
        shipped.complete();
      }
      if (body.contains('delivered') && !delivered.isCompleted) {
        debugPrint('PUSH_SMOKE_PUSH=delivered');
        delivered.complete();
      }
      unawaited(adapter.setConnected(false)); // presence gate, see above
    });

    // ---- go offline, then hand control to the script (the vendor) --------
    await adapter.setConnected(false);
    debugPrint('PUSH_SMOKE_ORDER_READY');
    try {
      await shipped.future.timeout(const Duration(minutes: 4));
      await delivered.future.timeout(const Duration(minutes: 4));
      debugPrint('PUSH_SMOKE_ORDER_PASS');
    } finally {
      await sub.cancel();
      // No adapter close here: its SQLite store is the app's own, and
      // db.close() after pauseSync is known to hang (see push_smoke_test's
      // ponytail note) — process exit reaps the smoke run.
    }
  });
}
