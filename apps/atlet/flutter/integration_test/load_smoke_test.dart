// Direct-mode LOAD smoke (ADR-0045/0049): sign in, open the same store the
// app opens (openNostosDirect), and report whether products ever arrive.
// Prints LOAD_SMOKE_* markers; run with the atlet defines file:
//   fvm flutter test integration_test/load_smoke_test.dart -d <device> \
//     --dart-define-from-file=<atlet-defines.json>

import 'dart:async';

import 'package:atlet/adapters/nostos_adapter.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:path_provider/path_provider.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

const _supabaseUrl = String.fromEnvironment('SUPABASE_URL');
const _supabaseAnonKey = String.fromEnvironment('SUPABASE_ANON_KEY');
const _email = String.fromEnvironment(
  'ATLET_SMOKE_EMAIL',
  defaultValue: 'flutter@atlet.dev',
);
const _password = String.fromEnvironment(
  'ATLET_SMOKE_PASSWORD',
  defaultValue: 'atlet-flutter-2026',
);

Future<void> main() async {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  binding.defaultTestTimeout = const Timeout(Duration(minutes: 3));

  testWidgets('direct-mode load smoke', (tester) async {
    await tester.pumpWidget(const SizedBox.shrink());
    await Supabase.initialize(
      url: _supabaseUrl,
      publishableKey: _supabaseAnonKey,
    );
    final auth = Supabase.instance.client.auth;
    final res = await auth.signInWithPassword(
      email: _email,
      password: _password,
    );
    final session = res.session!;
    debugPrint('LOAD_SMOKE_SIGNED_IN sub=${session.user.id}');

    final dir = await getApplicationDocumentsDirectory();
    final db = await openNostosDirect(
      supabaseUrl: _supabaseUrl,
      anonKey: _supabaseAnonKey,
      accessToken: session.accessToken,
      userId: session.user.id,
      dbDir: dir.path,
    );
    debugPrint('LOAD_SMOKE_OPENED path=${dir.path}');

    final states = db.connectionState.listen(
      (s) => debugPrint('LOAD_SMOKE_STATE $s'),
    );
    await db.subscribeTables(const [
      NostosTableSub(name: 'sessions'),
      NostosTableSub(name: 'products'),
      NostosTableSub(name: 'cart_items'),
      NostosTableSub(name: 'orders'),
      NostosTableSub(name: 'order_events'),
    ]);
    debugPrint('LOAD_SMOKE_SUBSCRIBED');
    final counts = db
        .watch('SELECT count(*) AS n FROM products')
        .listen((rows) => debugPrint('LOAD_SMOKE_PRODUCTS ${rows.first['n']}'));

    try {
      await db.waitForFirstSync().timeout(const Duration(seconds: 60));
      debugPrint('LOAD_SMOKE_FIRST_SYNC ok');
    } on TimeoutException {
      debugPrint('LOAD_SMOKE_FIRST_SYNC TIMEOUT');
    }
    await Future<void>.delayed(const Duration(seconds: 5));
    await states.cancel();
    await counts.cancel();
    await db.close();
    debugPrint('LOAD_SMOKE_DONE');
  });
}
