import 'package:flutter_test/flutter_test.dart';
import 'package:atlet/adapters/nostos_adapter.dart';
import 'package:atlet/adapters/sync_adapter.dart';

// NostosAdapter.init() calls Nostos.connect() which requires RustLib.init()
// (the native FFI bridge) — not available under `flutter test`'s host VM.
// Live init/signOut/write behavior is exercised by the docker-compose.atlet
// profile per task-9-brief's Step (checklist items 1, 4, 5), not here. This
// file covers the pure row-mapping/payload functions that gate correctness
// of the serverAcked mark and the NostosColumn affinities declared in
// NostosAdapter's schema.
void main() {
  group('NostosAdapter type', () {
    test('implements SyncAdapter with engine == cairn', () {
      final SyncAdapter adapter = NostosAdapter();
      expect(adapter.engine, 'cairn');
    });
  });

  group('sessionFromRow', () {
    test('parses a fully-populated row', () {
      final row = {
        'id': 's1',
        'title': 'Morning Run',
        'type': 'cardio',
        'metric': 5000,
        'unit': 'm',
        'note': 'felt good',
        'streak': 3,
        'occurred_on': '2026-08-06',
        'server_committed_at': '2026-08-06T12:00:00Z',
      };
      final s = sessionFromRow(row);
      expect(s.id, 's1');
      expect(s.metric, 5000);
      expect(s.note, 'felt good');
      expect(s.streak, 3);
      expect(s.occurredOn, DateTime.parse('2026-08-06'));
      expect(s.serverCommittedAt, isNotNull);
    });

    test('server_committed_at absent -> null (localVisible, not acked)', () {
      final row = {
        'id': 's1',
        'title': 'Run',
        'type': 'cardio',
        'metric': 5000,
        'unit': 'm',
        'occurred_on': '2026-08-06',
      };
      expect(sessionFromRow(row).serverCommittedAt, isNull);
    });

    test('server_committed_at explicit null -> null', () {
      final row = {
        'id': 's1',
        'title': 'Run',
        'type': 'cardio',
        'metric': 5000,
        'unit': 'm',
        'occurred_on': '2026-08-06',
        'server_committed_at': null,
      };
      expect(sessionFromRow(row).serverCommittedAt, isNull);
    });

    test('metric/streak arrive as num (SQLite REAL affinity coercion)', () {
      final row = {
        'id': 's1',
        'title': 'Run',
        'type': 'cardio',
        'metric': 5000.0,
        'unit': 'm',
        'streak': 2.0,
        'occurred_on': '2026-08-06',
      };
      final s = sessionFromRow(row);
      expect(s.metric, 5000);
      expect(s.streak, 2);
    });

    test('streak absent defaults to 0', () {
      final row = {
        'id': 's1',
        'title': 'Run',
        'type': 'cardio',
        'metric': 5000,
        'unit': 'm',
        'occurred_on': '2026-08-06',
      };
      expect(sessionFromRow(row).streak, 0);
    });
  });

  group('productFromRow', () {
    test('plant_based coerces from int/string/bool', () {
      Map<String, dynamic> base(Object plantBased) => {
            'id': 'p1',
            'name': 'Oat milk',
            'category': 'dairy',
            'price_cents': 399,
            'plant_based': plantBased,
          };
      expect(productFromRow(base(true)).plantBased, true);
      expect(productFromRow(base(1)).plantBased, true);
      expect(productFromRow(base('true')).plantBased, true);
      expect(productFromRow(base(0)).plantBased, false);
      expect(productFromRow(base('0')).plantBased, false);
    });

    test('rating handles num, String, and null', () {
      Map<String, dynamic> base(Object? rating) => {
            'id': 'p1',
            'name': 'Oat milk',
            'category': 'dairy',
            'price_cents': 399,
            'plant_based': true,
            'rating': rating,
          };
      expect(productFromRow(base(4.5)).rating, 4.5);
      expect(productFromRow(base('4.5')).rating, 4.5);
      expect(productFromRow(base(null)).rating, isNull);
    });

    test('price_cents arrives as num', () {
      final row = {
        'id': 'p1',
        'name': 'Oat milk',
        'category': 'dairy',
        'price_cents': 399.0,
        'plant_based': false,
      };
      expect(productFromRow(row).priceCents, 399);
    });
  });

  group('sessionWritePayload', () {
    final s = SessionRow(
      id: 's1',
      title: 'Run',
      type: 'cardio',
      metric: 5000,
      unit: 'm',
      streak: 2,
      occurredOn: DateTime(2026, 8, 6),
    );

    test('omits server_committed_at and user_id', () {
      final payload = sessionWritePayload(s);
      expect(payload.containsKey('server_committed_at'), isFalse);
      expect(payload.containsKey('user_id'), isFalse);
    });

    test('formats occurred_on as date-only', () {
      expect(sessionWritePayload(s)['occurred_on'], '2026-08-06');
    });

    test('omits note when null, includes when present', () {
      expect(sessionWritePayload(s).containsKey('note'), isFalse);
      final withNote = SessionRow(
        id: 's1',
        title: 'Run',
        type: 'cardio',
        metric: 5000,
        unit: 'm',
        note: 'hi',
        occurredOn: DateTime(2026, 8, 6),
      );
      expect(sessionWritePayload(withNote)['note'], 'hi');
    });
  });
}
