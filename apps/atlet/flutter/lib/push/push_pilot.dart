import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/foundation.dart';
import 'package:path_provider/path_provider.dart';

import '../adapters/nostos_adapter.dart';
import 'push_pilot_web_stub.dart'
    if (dart.library.js_interop) 'push_pilot_web.dart';

/// PILOT (ADR-0037) — push rails for the Atlet pilot: FCM on mobile, raw
/// Web Push (VAPID, no Firebase) on web — see push_pilot_web.dart.
///
/// Opt-in via `--dart-define=ATLET_PUSH_PILOT=true` (see main.dart): Firebase
/// needs platform config (google-services.json / GoogleService-Info.plist)
/// that is operator-owned and NOT checked in, so the whole module stays
/// dormant without the flag — analyze/test/build stay green with no Firebase
/// config present.
///
/// Push is a doorbell, not a data channel (docs/api/push.md): the data
/// payload is at most `{table, lsn}`; the wake target is always the sync
/// connection (resume / cold-open), never the push rail.

/// Same consts as main.dart's — keep them in sync. Both read the same
/// dart-defines, so they can only disagree if the defaults are edited apart.
const String _supabaseUrl = String.fromEnvironment(
  'SUPABASE_URL',
  defaultValue: 'https://PROJECT_REF.supabase.co',
);
const String _supabaseAnonKey = String.fromEnvironment(
  'SUPABASE_ANON_KEY',
  defaultValue: 'PLACEHOLDER_ANON_KEY',
);

/// A nostos doorbell: server mode's `{table, lsn}` (ADR-0037 §2), or direct
/// mode's `{cairn: ring}` from the `cairn-push` Edge Function (ADR-0045).
/// Pure so the routing decision is unit-testable (push_pilot_test.dart) —
/// everything non-Firebase that handles a [RemoteMessage] flows through it.
bool isNostosDoorbell(Map<String, dynamic>? data) =>
    data != null &&
    ((data.containsKey('table') && data.containsKey('lsn')) ||
        data['cairn'] == 'ring');

// Action pushes (`{title, body, category}` data, ADR-0037 §2 `action` mode)
// never render from Dart: iOS draws the system alert + the category's
// registered buttons from the FCM apns override (Runner/AppDelegate.swift),
// and Android's AtletMessagingService posts the action notification
// natively BEFORE Flutter sees the message — foreground, background, or
// killed. Dart only observes them (e.g. the order-leg smoke's onMessage
// assertions).

/// Name of the file [PushPilot.attach] drops next to the SQLite store so the
/// background-isolate handler can re-auth its cold-open. Never a secret
/// beyond what supabase_flutter already persists on disk for the session.
const String _sessionFileName = 'nostos_push_pilot_session.json';

/// Background doorbell wake — mirrors sdk/nostos_flutter README's "Wake
/// entry" snippet. Runs on its own isolate (its own engine), so the FRB
/// handle of the foreground session cannot cross; the durable LSN checkpoint
/// is what makes a cold-open cheap (delta applies, not a resync).
///
/// ponytail: the access token is whatever attach() last persisted — it can
/// be stale (Supabase access tokens live ~1h), in which case the pull 401s,
/// the first sync never lands, and this wake times out as a no-op (the
/// timeout also keeps it inside the OS's ~30s background budget).
/// Harmless: a doorbell is a hint,
/// and the next foreground app-open syncs regardless. Upgrade: refresh the
/// token from Supabase inside this isolate when a no-op wake is ever
/// observed in practice.
@pragma('vm:entry-point')
Future<void> nostosDoorbellBackgroundHandler(RemoteMessage message) async {
  // Action pushes are rendered natively (see the module doc above) — nothing
  // for this isolate to do with them.
  if (!isNostosDoorbell(message.data)) return;
  debugPrint('push pilot: background doorbell ${message.data}');
  final dir = await getApplicationDocumentsDirectory();
  final file = File('${dir.path}/$_sessionFileName');
  if (!await file.exists()) return; // pilot never attached — nothing to wake
  final creds = jsonDecode(await file.readAsString());
  if (creds is! Map<String, dynamic>) return;
  final (token, userId) = (creds['accessToken'], creds['userId']);
  if (token is! String || userId is! String) return; // pre-direct file
  try {
    // SAME file and scope as the foreground session (the live app is
    // direct-only; the server-mode adapter is the bench's).
    final db = await openNostosDirect(
      supabaseUrl: _supabaseUrl,
      anonKey: _supabaseAnonKey,
      accessToken: token,
      userId: userId,
      dbDir: dir.path,
    );
    try {
      // Direct mode pulls the whole scope; the table only satisfies the API.
      await db.subscribe('orders');
      await db.waitForFirstSync().timeout(const Duration(seconds: 20));
    } finally {
      await db.close(); // NOT signOut() — the local store must survive
    }
  } catch (e) {
    // Best-effort wake: see the ponytail above. Log and leave the durable
    // checkpoint to reconcile on the next foreground session.
    debugPrint('push pilot: background wake failed: $e');
  }
}

/// Owns the FCM registration lifecycle for the nostos engine. One module-level
/// instance (like [engineRegistry] in main.dart) — see [pushPilot].
class PushPilot {
  NostosAdapter? _adapter;
  StreamSubscription<String>? _refreshSub;
  StreamSubscription<RemoteMessage>? _messageSub;

  /// Registers this device's FCM token against the live nostos engine and
  /// wires the foreground doorbell → `resumeSync()` wake. Call after every
  /// successful nostos engine start (initial + switches) — the SDK deregisters
  /// session-registered tokens on `signOut()`, so each new session must
  /// re-register.
  Future<void> attach(NostosAdapter adapter) async {
    _adapter = adapter;
    // Web arm: raw Web Push against cairn's own rail (ADR-0037 §1) — no
    // Firebase objects exist here, the service worker owns delivery.
    if (kIsWeb) {
      try {
        await attachWebPush(adapter, debugPrint);
      } on Exception catch (e) {
        // Covers NostosPushTokenException (server rejected) and the browser
        // http ClientException (server unreachable) — registration retries
        // on the next attach, same contract as the mobile arm.
        debugPrint('push pilot: web token registration failed: $e');
      }
      return;
    }
    await _messageSub?.cancel();
    _messageSub = FirebaseMessaging.onMessage.listen((message) {
      if (!isNostosDoorbell(message.data)) return;
      debugPrint('push pilot: foreground doorbell ${message.data}');
      // Resume path: the engine's own reconnect + LSN checkpoint delivers
      // the delta. Errors swallowed — a failed resume falls back to the
      // engine's backoff loop, same as the connectivity guard's.
      unawaited(adapter.setConnected(true));
    });

    final messaging = FirebaseMessaging.instance;
    // requestPermission is a no-op-ish on Android (granted at install for
    // data-only messages) and required on iOS before any token exists.
    await messaging.requestPermission();
    final token = await messaging.getToken();
    if (token == null || token.isEmpty) {
      debugPrint('push pilot: no FCM token (Firebase config / platform?)');
      return;
    }
    await _register(token);

    await _refreshSub?.cancel();
    _refreshSub = messaging.onTokenRefresh.listen((token) {
      final current = _adapter;
      if (current != null) unawaited(_registerOn(current, token));
    });
  }

  /// Stops forwarding doorbells. Token DEREGISTRATION is not done here — the
  /// SDK's sign-out hook owns it (ADR-0037 §3).
  Future<void> detach() async {
    _adapter = null;
    await _messageSub?.cancel();
    _messageSub = null;
    await _refreshSub?.cancel();
    _refreshSub = null;
    if (kIsWeb) return; // no session file — the bg-isolate wake is FCM/mobile
    final dir = await getApplicationDocumentsDirectory();
    final file = File('${dir.path}/$_sessionFileName');
    // Best-effort: a stale file only ever produces the bg handler's
    // documented no-op wake.
    try {
      await file.delete();
    } on FileSystemException {
      // absent — fine
    }
  }

  Future<void> _register(String token) async {
    final adapter = _adapter;
    if (adapter == null) return;
    await _registerOn(adapter, token);
  }

  Future<void> _registerOn(NostosAdapter adapter, String token) async {
    try {
      await adapter.registerPushToken('fcm', token);
      debugPrint('push pilot: FCM token registered');
      // Seed the background-isolate wake (see nostosDoorbellBackgroundHandler).
      final dir = await getApplicationDocumentsDirectory();
      await File('${dir.path}/$_sessionFileName').writeAsString(
        jsonEncode({
          'accessToken': adapter.currentAccessToken,
          'userId': adapter.currentUserId,
        }),
      );
    } on NostosPushTokenException catch (e) {
      // Non-fatal: registration retries on the next attach()/token refresh.
      debugPrint('push pilot: registerPushToken failed: $e');
    }
  }
}

/// Module-level singleton, alive for the app's lifetime (mirrors main.dart's
/// `engineRegistry`). Only touched when ATLET_PUSH_PILOT is set.
final PushPilot pushPilot = PushPilot();
