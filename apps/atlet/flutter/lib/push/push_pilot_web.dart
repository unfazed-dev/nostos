/// Web arm of the Atlet push pilot — raw Web Push over cairn's own rail
/// (ADR-0037 §1: direct VAPID, no Firebase intermediary on the web).
///
/// Compiled ONLY on web: push_pilot.dart's conditional import routes here via
/// `dart.library.js_interop` (the same selection idiom as the SDK's
/// engine_selector.dart, ADR-0036), so native builds never see `package:web`.
///
/// Flow: Notification permission → register web/atlet-push-sw.js →
/// `pushManager.subscribe` with the operator's VAPID public key
/// (`--dart-define=ATLET_VAPID_PUBLIC_KEY=…`) → register the browser
/// `pushSubscription` JSON as platform `'webpush'` (exactly the token the
/// server's WebPushRail parses — `{endpoint, keys:{p256dh, auth}}`).
///
/// There is no foreground doorbell listener here: the service worker owns
/// push delivery, and while the page is open the live sync socket carries
/// the update (the offline gate suppresses pushes by design).
library;

import 'dart:convert';
import 'dart:js_interop';
import 'dart:typed_data';

import 'package:web/web.dart' as web;

import '../../adapters/nostos_adapter.dart';

/// VAPID public key (base64url of the 65-byte uncompressed P-256 point) —
/// must be the better half of the server's NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY.
const String _vapidPublicKey = String.fromEnvironment('ATLET_VAPID_PUBLIC_KEY');

Future<void> attachWebPush(NostosAdapter adapter, void Function(String) log) async {
  if (_vapidPublicKey.isEmpty) {
    log('push pilot (web): ATLET_VAPID_PUBLIC_KEY not set — skipping subscribe');
    return;
  }
  final permission = await web.Notification.requestPermission().toDart;
  if (permission.toDart != 'granted') {
    log('push pilot (web): notification permission $permission — no push');
    return;
  }
  final registration = await web
      .window.navigator.serviceWorker
      .register('./atlet-push-sw.js'.toJS)
      .toDart;
  final pushManager = registration.pushManager;

  // Reuse the existing subscription unless it was minted against a different
  // VAPID key — subscribe() throws InvalidStateError in that case, so retry
  // once after unsubscribe (the browser keys the subscription to the key).
  web.PushSubscription subscription;
  final options = web.PushSubscriptionOptionsInit(
    userVisibleOnly: true,
    applicationServerKey: _applicationServerKey(_vapidPublicKey).toJS,
  );
  try {
    subscription = await pushManager.subscribe(options).toDart;
  } catch (_) {
    final stale = await pushManager.getSubscription().toDart;
    await stale?.unsubscribe().toDart;
    subscription = await pushManager.subscribe(options).toDart;
  }

  final json = subscription.toJSON();
  // dartify() of a JS object yields Map<dynamic, dynamic> — the old
  // `is Map<String, dynamic>` guard never matched, so `keys` serialized as {}
  // and the server's WebPushRail rejected the token ("missing field
  // `p256dh`"). Re-key to <String, String> instead.
  final keys = json.keys.dartify();
  final token = jsonEncode({
    'endpoint': json.endpoint,
    'keys': keys is Map
        ? Map<String, String>.from(keys)
        : const <String, String>{},
  });
  await adapter.registerPushToken('webpush', token);
  log('push pilot (web): webpush subscription registered');
}

/// base64url → the raw bytes `applicationServerKey` wants (BufferSource).
Uint8List _applicationServerKey(String b64url) {
  final normalized = b64url.replaceAll('-', '+').replaceAll('_', '/');
  final padded =
      normalized + '=' * ((4 - normalized.length % 4) % 4);
  return base64.decode(padded);
}
