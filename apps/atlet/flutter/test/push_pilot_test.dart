import 'package:flutter_test/flutter_test.dart';
import 'package:atlet/push/push_pilot.dart';

/// Pure-logic check for the doorbell routing decision — the only non-Firebase
/// behavior in the pilot module (everything else needs a live FCM rail).
void main() {
  group('isNostosDoorbell', () {
    test('accepts a nostos doorbell payload {table, lsn}', () {
      expect(
        isNostosDoorbell({'table': 'sessions', 'lsn': '0/1A2B3C'}),
        isTrue,
      );
    });

    test('accepts the direct-mode ring {nostos: ring} (ADR-0045)', () {
      expect(isNostosDoorbell({'nostos': 'ring'}), isTrue);
      expect(isNostosDoorbell({'nostos': 'other'}), isFalse);
    });

    test('rejects payloads missing either key', () {
      expect(isNostosDoorbell({'table': 'sessions'}), isFalse);
      expect(isNostosDoorbell({'lsn': '0/1A2B3C'}), isFalse);
      expect(isNostosDoorbell(<String, dynamic>{}), isFalse);
    });

    test('rejects null and non-doorbell messages', () {
      expect(isNostosDoorbell(null), isFalse);
      expect(
        isNostosDoorbell({
          'notification': {'title': 'sale'},
        }),
        isFalse,
      );
    });
  });
}
