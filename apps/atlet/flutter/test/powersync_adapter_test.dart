import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:atlet/adapters/powersync_adapter.dart';
import 'package:atlet/adapters/sync_adapter.dart';

// PowerSyncAdapter.init() calls PowerSyncDatabase.initialize()/connect(),
// which need the native powersync sqlite extension — not available under
// `flutter test`'s host VM. Live init/signOut/write behavior is exercised by
// the docker-compose.atlet profile per task-10-brief's Step (checklist items
// 1, 2, 4, 5), not here. This file covers the pure row-mapping/payload
// functions and the listen-before-connect ordering guard, mirroring
// nostos_adapter_test.dart's coverage of the equivalent NostosAdapter pieces.
void main() {
  group('PowerSyncAdapter type', () {
    test('implements SyncAdapter with engine == powersync', () {
      final SyncAdapter adapter = PowerSyncAdapter();
      expect(adapter.engine, 'powersync');
    });
  });

  group('wireConnected (init() ordering regression)', () {
    // Fixture reproduces db.statusStream's real contract: a broadcast stream
    // with nothing emitted until connect() runs, and no replay to a listener
    // that attaches afterward (StreamController.broadcast drops .add() calls
    // made with zero current listeners) — same hazard nostos_adapter_test.dart
    // guards against for NostosDatabase.connectionState.
    test('a listener attached AFTER connect misses the transition (the bug)',
        () async {
      final controller = StreamController<bool>.broadcast();
      final seen = <bool>[];

      controller.add(true); // "connect" fires

      final sub = wireConnected(controller.stream, seen.add);
      await Future<void>.delayed(Duration.zero);

      expect(seen, isEmpty);
      await sub.cancel();
      await controller.close();
    });

    test(
        'a listener attached BEFORE connect surfaces initial connected=true (the fix)',
        () async {
      final controller = StreamController<bool>.broadcast();
      final seen = <bool>[];

      final sub = wireConnected(controller.stream, seen.add);
      controller.add(true); // "connect" fires
      await Future<void>.delayed(Duration.zero);

      expect(seen, [true]);
      await sub.cancel();
      await controller.close();
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

    test('includes id, omits server_committed_at and user_id', () {
      final payload = sessionWritePayload(s);
      expect(payload['id'], 's1');
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
