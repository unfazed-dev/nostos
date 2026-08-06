// Tests for lib/ui/analytics.dart (task-15): pure metric-row shaping plus
// widget tests using injected fakes for store/runSuite/uploadRuns, same
// pattern as shop_test.dart's fake SyncAdapter — no live Supabase/adapter
// needed.

import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/bench/runner.dart';
import 'package:atlet/bench/store.dart';
import 'package:atlet/ui/analytics.dart';

/// Lets [BenchStore]'s real `dart:io` File operations actually complete,
/// then pumps a frame to rebuild. `tester.pump(duration)` alone is not
/// enough here: it only advances flutter_test's fake clock and flushes
/// already-queued microtasks — it never yields to the real event loop, so a
/// real (non-Timer) File-read Future can be left forever unresolved no
/// matter how many bounded pumps follow. `tester.runAsync()` is the
/// documented escape hatch for exactly this (real I/O / real Futures) and is
/// also why `pumpAndSettle()` is doubly wrong here: on top of that gap, it
/// additionally never converges while AnalyticsScreen's indeterminate
/// CircularProgressIndicators keep scheduling frames.
Future<void> _settle(WidgetTester tester) async {
  for (var i = 0; i < 10; i++) {
    await tester.runAsync(
      () => Future<void>.delayed(const Duration(milliseconds: 20)),
    );
    await tester.pump();
  }
}

RunRecord _fixture({
  required String engine,
  required String runType,
  required Map<String, dynamic> metrics,
  DateTime? startedAt,
}) => RunRecord(
  sdk: 'flutter',
  engine: engine,
  profile: 'local',
  runType: runType,
  specVersion: '1.0',
  seedSize: 100,
  appVersion: '0.1.0',
  device: {'model': 'test'},
  metrics: metrics,
  startedAt: startedAt ?? DateTime.utc(2026, 8, 1),
);

void main() {
  group('metricRowFor', () {
    test('propagation carries both median and p95', () {
      final row = metricRowFor(
        _fixture(
          engine: 'cairn',
          runType: 'propagation',
          metrics: {
            'propagation_ms_median': 12.5,
            'propagation_ms_p95': 40.0,
          },
        ),
      );
      expect(row.value, 12.5);
      expect(row.p95, 40.0);
      expect(row.unit, 'ms');
    });

    test('cold_sync has no p95 (single sample)', () {
      final row = metricRowFor(
        _fixture(
          engine: 'powersync',
          runType: 'cold_sync',
          metrics: {'cold_sync_ms': 900},
        ),
      );
      expect(row.value, 900);
      expect(row.p95, isNull);
    });

    test('db_bytes reports bytes with no p95', () {
      final row = metricRowFor(
        _fixture(
          engine: 'cairn',
          runType: 'db_bytes',
          metrics: {'db_bytes': 204800, 'journal_mode': 'wal'},
        ),
      );
      expect(row.value, 204800);
      expect(row.unit, 'bytes');
      expect(row.p95, isNull);
    });

    test('unknown run type throws rather than guessing a metric key', () {
      expect(
        () => metricRowFor(
          _fixture(engine: 'cairn', runType: 'mystery', metrics: {}),
        ),
        throwsArgumentError,
      );
    });
  });

  group('latestMetricRows', () {
    test('collapses repeated (engine, run_type) runs to the latest by startedAt', () {
      final rows = latestMetricRows([
        _fixture(
          engine: 'cairn',
          runType: 'cold_sync',
          metrics: {'cold_sync_ms': 100},
          startedAt: DateTime.utc(2026, 1, 1),
        ),
        _fixture(
          engine: 'cairn',
          runType: 'cold_sync',
          metrics: {'cold_sync_ms': 50},
          startedAt: DateTime.utc(2026, 1, 2),
        ),
      ]);
      expect(rows, hasLength(1));
      expect(rows.single.value, 50);
    });

    test('sorts by engine then canonical run-type order', () {
      final rows = latestMetricRows([
        _fixture(
          engine: 'powersync',
          runType: 'write_ack',
          metrics: {'write_ack_ms_median': 1, 'write_ack_ms_p95': 2},
        ),
        _fixture(
          engine: 'cairn',
          runType: 'queue_drain',
          metrics: {'queue_drain_ms': 5},
        ),
        _fixture(
          engine: 'cairn',
          runType: 'cold_sync',
          metrics: {'cold_sync_ms': 5},
        ),
      ]);
      expect(rows.map((r) => '${r.engine}/${r.runType}').toList(), [
        'cairn/cold_sync',
        'cairn/queue_drain',
        'powersync/write_ack',
      ]);
    });
  });

  group('AnalyticsScreen', () {
    late Directory tempDir;
    late BenchStore store;

    setUp(() async {
      tempDir = await Directory.systemTemp.createTemp('atlet_analytics_test_');
      store = BenchStore(directory: tempDir);
    });

    tearDown(() async {
      if (await tempDir.exists()) await tempDir.delete(recursive: true);
    });

    testWidgets('renders the permanent internal-eval banner', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: AnalyticsScreen(
            store: store,
            uploadRuns: (rows) async {},
            runSuite: () async {},
          ),
        ),
      );
      await tester.pump();
      expect(find.byKey(const Key('analytics-eval-banner')), findsOneWidget);
      expect(find.text(RunRecord.evaluationLabel), findsOneWidget);
    });

    testWidgets('empty store shows the empty state, not a table', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: AnalyticsScreen(
            store: store,
            uploadRuns: (rows) async {},
            runSuite: () async {},
          ),
        ),
      );
      await _settle(tester);
      expect(find.text('No runs yet.'), findsOneWidget);
      expect(find.byKey(const Key('results-table')), findsNothing);
    });

    testWidgets('run-suite button invokes runSuite and reloads the table', (tester) async {
      var runCount = 0;
      Future<void> fakeRunSuite() async {
        runCount++;
        await store.append(
          _fixture(
            engine: 'cairn',
            runType: 'cold_sync',
            metrics: {'cold_sync_ms': 77},
          ),
        );
      }

      await tester.pumpWidget(
        MaterialApp(
          home: AnalyticsScreen(
            store: store,
            uploadRuns: (rows) async {},
            runSuite: fakeRunSuite,
          ),
        ),
      );
      await _settle(tester);

      await tester.tap(find.byKey(const Key('run-suite-button')));
      await _settle(tester);

      expect(runCount, 1);
      expect(find.byKey(const Key('results-table')), findsOneWidget);
      expect(find.byKey(const Key('result-row-cairn-cold_sync')), findsOneWidget);
    });

    testWidgets('upload button posts stored runs and reports the count', (tester) async {
      // Real dart:io File I/O — testWidgets wraps the *entire* callback (not
      // just post-pumpWidget) in a FakeAsync zone, so even a setup-time
      // `store.append` needs `runAsync` or it never completes (see _settle's
      // doc comment above for why plain pump-based waiting can't help here
      // either).
      await tester.runAsync(
        () => store.append(
          _fixture(
            engine: 'cairn',
            runType: 'cold_sync',
            metrics: {'cold_sync_ms': 10},
          ),
        ),
      );

      List<Map<String, dynamic>>? captured;
      await tester.pumpWidget(
        MaterialApp(
          home: AnalyticsScreen(
            store: store,
            uploadRuns: (rows) async {
              captured = rows;
            },
            runSuite: () async {},
          ),
        ),
      );
      await _settle(tester);

      await tester.tap(find.byKey(const Key('upload-button')));
      await _settle(tester);

      expect(captured, isNotNull);
      expect(captured!.length, 1);
      expect(find.text('Uploaded 1 run(s).'), findsOneWidget);
    });

    testWidgets('a failing upload surfaces the error instead of crashing', (tester) async {
      await tester.runAsync(
        () => store.append(
          _fixture(
            engine: 'cairn',
            runType: 'cold_sync',
            metrics: {'cold_sync_ms': 10},
          ),
        ),
      );

      await tester.pumpWidget(
        MaterialApp(
          home: AnalyticsScreen(
            store: store,
            uploadRuns: (rows) async {
              throw StateError('network down');
            },
            runSuite: () async {},
          ),
        ),
      );
      await _settle(tester);

      await tester.tap(find.byKey(const Key('upload-button')));
      await _settle(tester);

      expect(tester.takeException(), isNull);
      expect(find.textContaining('Upload failed'), findsOneWidget);
    });
  });
}
