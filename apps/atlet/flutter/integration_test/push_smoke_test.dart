// PILOT (ADR-0037) — real-rail push smoke, driven by tool/push_smoke.sh.
//
// Proves the full doorbell path on the FCM rail:
//   app signs in → registers its FCM token via POST /push-tokens → goes
//   offline (pauseSync: doorbells only target offline accounts) → harness
//   inserts a `sessions` row into the local docker PG → nostos-server
//   replicates → doorbell → FCM → THIS test's onMessage fires.
//
// The harness asserts the server side (cairn_push_sent_total on /metrics);
// this test asserts the device side (message received + doorbell payload).
//
// Dart-defines (all passed by the harness; see tool/PUSH_SMOKE.md):
//   SUPABASE_URL / SUPABASE_ANON_KEY — the atlet Supabase project (auth).
//   NOSTOS_SYNC_URL — the smoke nostos-server (default ws://10.0.2.2:8080/sync;
//     10.0.2.2 is the Android emulator's alias for the host's loopback).
//   ATLET_SMOKE_EMAIL / ATLET_SMOKE_PASSWORD — seeded user (same defaults as
//     the SigninScreen prefill).

import 'dart:async';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/widgets.dart';
import 'package:path_provider/path_provider.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import 'package:atlet/push/push_pilot.dart';

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
const _nostosUrl = String.fromEnvironment(
  'NOSTOS_SYNC_URL',
  defaultValue: 'ws://10.0.2.2:8080/sync',
);

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  if (_supabaseUrl.isEmpty || _supabaseAnonKey.isEmpty) {
    // The harness pre-checks these; landing here means it was invoked
    // directly without them. Fail loudly with the fix, not a mystery hang.
    throw StateError(
      'SUPABASE_URL / SUPABASE_ANON_KEY dart-defines required — '
      'run via apps/atlet/flutter/tool/push_smoke.sh',
    );
  }

  await Firebase.initializeApp(); // platform config (google-services.json …)
  await Supabase.initialize(url: _supabaseUrl, publishableKey: _supabaseAnonKey);

  // (b) of the smoke contract: capture the FCM token the app obtains.
  await FirebaseMessaging.instance.requestPermission();
  final fcmToken = await FirebaseMessaging.instance.getToken();
  debugPrint('PUSH_SMOKE_TOKEN=$fcmToken');
  if (fcmToken == null || fcmToken.isEmpty) {
    throw StateError(
      'no FCM token — platform Firebase config missing, or an iOS simulator '
      '(which cannot receive real FCM; see tool/PUSH_SMOKE.md)',
    );
  }

  final auth = Supabase.instance.client.auth;
  await auth.signInWithPassword(email: _email, password: _password);
  final liveSession = auth.currentSession;
  if (liveSession == null) throw StateError('sign-in produced no session');
  debugPrint('PUSH_SMOKE_USER=${liveSession.user.id}');

  final dir = await getApplicationDocumentsDirectory();
  // Scratch store — NOT the real app's cairn.sqlite; the smoke is disposable.
  final db = await NostosDatabase.connect(
    url: _nostosUrl,
    token: liveSession.accessToken,
    sqlitePath: '${dir.path}/nostos_push_smoke.sqlite',
  );
  await db.subscribeTables(const [NostosTableSub(name: 'sessions')]);
  await db.registerPushToken('fcm', fcmToken);
  await db.waitForFirstSync();

  // Doorbells are suppressed for online accounts (fanout presence check) —
  // go offline so the harness's row insert targets us.
  await db.pauseSync();

  final received = Completer<void>();
  final sub = FirebaseMessaging.onMessage.listen((message) {
    debugPrint('PUSH_SMOKE_MESSAGE data=${message.data}');
    if (isNostosDoorbell(message.data) && !received.isCompleted) {
      debugPrint('PUSH_SMOKE_RECEIVED table=${message.data['table']} '
          'lsn=${message.data['lsn']}');
      received.complete();
    }
  });

  // Marker LAST: the harness inserts the triggering row only after seeing it.
  debugPrint('PUSH_SMOKE_READY');
  try {
    await received.future.timeout(const Duration(minutes: 3));
    debugPrint('PUSH_SMOKE_PASS');
  } on TimeoutException {
    debugPrint('PUSH_SMOKE_TIMEOUT no doorbell within 3min of insert');
    rethrow;
  } finally {
    await sub.cancel();
    await db.close();
  }
}
