// Tests for lib/push/order_push.dart — the payload a notification carries and
// the id a tap resolves to. Pure functions: no platform, no engine.
//
// What breaks without them: a routing key renamed on one side of the
// MethodChannel, which looks like "the deep link just doesn't work".
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/push/order_push.dart';

OrderEventRow _event() => OrderEventRow(
  id: 'ev-1',
  orderId: '983979e8-4004-48b5-b397-8cd73442f45c',
  status: 'shipped',
  previousStatus: 'paid',
  createdAt: DateTime.utc(2026, 9, 23, 16, 40),
);

void main() {
  test('the routing keys live in data, never in the visible text', () {
    final payload = orderPushPayload(_event());
    expect(payload['body'], 'Order 983979e8 is shipped');
    final data = payload['data']! as Map<String, Object?>;
    expect(data['cairn_route'], '/history/ev-1');
    expect(data['deep_link'], 'atlet://history/ev-1');
    expect(data['event_id'], 'ev-1');
    expect(data['status'], 'shipped');
    expect(data['previous_status'], 'paid');
  });

  test('a tap resolves an id from data, flat or nested', () {
    final payload = orderPushPayload(_event());
    // The whole payload (local-notification userInfo round trip)…
    expect(tappedEventId(payload), 'ev-1');
    // …and the flat data map (FCM hands message.data straight over).
    expect(tappedEventId(payload['data']! as Map<String, Object?>), 'ev-1');
  });

  test('a route alone names the event — a URL carries nothing else', () {
    expect(tappedEventId({'cairn_route': '/history/ev-9'}), 'ev-9');
    expect(tappedEventId({'cairn_route': '/history/'}), isNull);
    expect(tappedEventId({'cairn_route': '/somewhere/else'}), isNull);
    expect(tappedEventId(const {}), isNull); // someone else's push
  });

  test('the delivery log keeps attempts per event, newest first', () {
    final a = PushAttempt(
      eventId: 'ev-1',
      at: DateTime.utc(2026, 9, 23, 16, 40),
      channel: 'local-banner',
      payload: const {},
    );
    final b = PushAttempt(
      eventId: 'ev-2',
      at: DateTime.utc(2026, 9, 23, 16, 41),
      channel: 'local-banner',
      payload: const {},
      error: 'not authorized',
    );
    expect(attemptsFor([b, a], 'ev-1'), [a]);
    expect(attemptsFor([b, a], 'ev-2').single.error, 'not authorized');
  });
}
