// What an order-status notification IS: one payload builder, one in-memory
// delivery log, and the route a tap resolves to.
//
// Split out of main.dart so the History detail screen shows the payload that
// was actually posted rather than a second rendering of it — two renderings
// drift, and a smoke test that reads the drifted one proves nothing.
import 'package:flutter/foundation.dart';

import '../adapters/sync_adapter.dart';

/// In-app route for one order event. Also the path half of [deepLinkFor].
String historyRoute(String eventId) => '/history/$eventId';

/// `atlet://history/<id>` — the external form, for a notification posted by
/// anything that is not this process (a real APNs/FCM push, `xcrun simctl
/// push`, a test script). Host `history`, path `/&lt;id&gt;`; both platforms also
/// accept the one-slash `atlet:/history/<id>` a human is likely to type.
///
/// ponytail: a custom scheme, not a Universal Link. A Universal Link needs an
/// apple-app-site-association file on a domain atlet does not own; the scheme
/// costs an Info.plist entry and works on the simulator today. Swap when atlet
/// has a domain.
String deepLinkFor(String eventId) => 'atlet:/${historyRoute(eventId)}';

/// The notification for one order event — exactly what goes over the
/// MethodChannel to the platform, and exactly what the detail screen shows.
///
/// `data` is where the routing keys live, never `title`/`body`: FCM only
/// delivers the data half reliably in all three app states (terminated,
/// background, foreground), so a deep link parked in the visible text is a
/// deep link that works only sometimes.
/// See https://firebase.google.com/docs/cloud-messaging/flutter/receive-messages
Map<String, Object?> orderPushPayload(OrderEventRow e) {
  final short = e.orderId.length >= 8 ? e.orderId.substring(0, 8) : e.orderId;
  return {
    'title': 'Atlet order update',
    'body': 'Order $short is ${e.status}',
    // Rendered by the platform as action buttons; registered in
    // AppDelegate.swift as `order_status` (ADR-0037 §2 `action` mode).
    'category': 'order_status',
    'data': {
      'cairn_route': historyRoute(e.id),
      'deep_link': deepLinkFor(e.id),
      'event_id': e.id,
      'order_id': e.orderId,
      'status': e.status,
      'previous_status': e.previousStatus,
      'occurred_at': e.createdAt.toUtc().toIso8601String(),
    },
  };
}

/// One attempt to put a payload in front of the user.
@immutable
class PushAttempt {
  const PushAttempt({
    required this.eventId,
    required this.at,
    required this.channel,
    required this.payload,
    this.error,
  });

  final String eventId;
  final DateTime at;

  /// `local-banner` (platform notification), `snackbar` (web has no
  /// MethodChannel), or `fcm` (a real push came back through the app).
  final String channel;
  final Map<String, Object?> payload;

  /// Null when the platform accepted the post. A banner the OS refused and a
  /// banner never asked for look identical from outside the app — this is the
  /// difference, written down.
  final String? error;
}

/// Every attempt this session, newest first.
///
/// ponytail: in memory, session-scoped. A durable log would need a table, a
/// retention rule and a sync decision; the EVENTS are already durable in
/// `order_events`, and what this adds — "did the banner actually go out just
/// now" — is a question about the current run.
final pushLog = ValueNotifier<List<PushAttempt>>(const []);

void recordPushAttempt(PushAttempt attempt) {
  pushLog.value = [attempt, ...pushLog.value];
}

/// Whether the app posts its own banner for an order event, or leaves it to
/// the server's templated push (`cairn.push_templates`, ADR-0037 §2b).
///
/// The push reaches a phone in every app state, open included: direct mode
/// sends no presence heartbeat (see `NostosDatabase.direct`), so the trigger
/// counts the device as absent, and AppDelegate's ForegroundBanner shows the
/// push while the app is in front. A banner of our own would be the second
/// copy. If a heartbeat ever ships, the open app gets no push, and this has
/// to become "post while in the foreground".
bool postsOwnBanner({required bool pushPilot, required bool web}) =>
    web || !pushPilot;

/// Attempts for one event, newest first.
List<PushAttempt> attemptsFor(List<PushAttempt> all, String eventId) =>
    all.where((a) => a.eventId == eventId).toList();

/// The event id a tapped notification points at, or null if the payload is not
/// one of ours.
///
/// Accepts both the flat `data` map (FCM hands `message.data` straight over)
/// and a whole payload with a nested `data` key (the local-notification
/// `userInfo` round trip), because the tap handler is one function and the two
/// sources genuinely differ.
String? tappedEventId(Map<Object?, Object?> data) {
  final nested = data['data'];
  final map = nested is Map ? nested : data;
  final id = map['event_id'];
  if (id is String && id.isNotEmpty) return id;
  // A route without an id still names one: /history/<id>.
  final route = map['cairn_route'];
  if (route is String && route.startsWith('/history/')) {
    final tail = route.substring('/history/'.length);
    if (tail.isNotEmpty) return tail;
  }
  return null;
}
