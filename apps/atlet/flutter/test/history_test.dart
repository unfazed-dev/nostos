// Tests for lib/ui/history.dart: the order-history feed and the two places a
// row can take you — the bench (app bar) and the event detail (row tap). The
// bench's own tests live in bench_test.dart.

import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/bench/store.dart';
import 'package:atlet/ui/history.dart';

/// Lets [BenchStore]'s real `dart:io` File operations actually complete,
/// then pumps a frame to rebuild. `tester.pump(duration)` alone is not
/// enough here: it only advances flutter_test's fake clock and flushes
/// already-queued microtasks — it never yields to the real event loop, so a
/// real (non-Timer) File-read Future can be left forever unresolved no
/// matter how many bounded pumps follow. `tester.runAsync()` is the
/// documented escape hatch for exactly this (real I/O / real Futures) and is
/// also why `pumpAndSettle()` is doubly wrong here: on top of that gap, it
/// additionally never converges while HistoryScreen's indeterminate
/// CircularProgressIndicators keep scheduling frames.
Future<void> _settle(WidgetTester tester) async {
  for (var i = 0; i < 10; i++) {
    await tester.runAsync(
      () => Future<void>.delayed(const Duration(milliseconds: 20)),
    );
    await tester.pump();
  }
}

void main() {
  group('historyLine', () {
    test('a first event reads as a plain status, a transition as an arrow', () {
      final created = OrderEventRow(
        id: 'e1',
        orderId: '983979e8-4004-48b5-b397-8cd73442f45c',
        status: 'pending',
        createdAt: DateTime.utc(2026, 9, 23, 16, 40),
      );
      expect(historyLine(created), '983979e8 · pending');
      expect(
        historyLine(
          OrderEventRow(
            id: 'e2',
            orderId: created.orderId,
            status: 'shipped',
            previousStatus: 'paid',
            createdAt: created.createdAt,
          ),
        ),
        '983979e8 · paid → shipped',
      );
    });
  });
  group('HistoryScreen', () {
    late Directory tempDir;
    late BenchStore store;

    setUp(() async {
      tempDir = await Directory.systemTemp.createTemp('atlet_history_test_');
      store = BenchStore(directory: tempDir);
    });

    tearDown(() async {
      if (await tempDir.exists()) await tempDir.delete(recursive: true);
    });

    testWidgets('stacks order events, and says so when there are none', (
      tester,
    ) async {
      // Two pumps rather than a StreamController: a controller's listener
      // outlives the pump in this file's `runAsync`-based settle and wedges
      // the whole test file (caught 2026-09-23 — every later test in the file
      // reported "did not complete"). A fresh stream per state proves the same
      // two renderings.
      Widget screen(Stream<List<OrderEventRow>> events) => MaterialApp(
        home: HistoryScreen(
          events: events,
          store: store,
          uploadRuns: (rows) async {},
          runSuite: () async {},
        ),
      );

      await tester.pumpWidget(screen(Stream.value(const [])));
      await _settle(tester);
      expect(find.byKey(const Key('history-feed-empty')), findsOneWidget);

      // The point of the tab: a status the user may have missed is still here
      // after the banner is gone.
      await tester.pumpWidget(
        screen(
          Stream.value([
            OrderEventRow(
              id: 'e2',
              orderId: '983979e8-4004-48b5-b397-8cd73442f45c',
              status: 'shipped',
              previousStatus: 'paid',
              createdAt: DateTime.utc(2026, 9, 23, 16, 40),
            ),
          ]),
        ),
      );
      await _settle(tester);
      expect(find.byKey(const Key('history-event-e2')), findsOneWidget);
      expect(find.text('983979e8 · paid → shipped'), findsOneWidget);
    });

    testWidgets('a row opens the detail view for that event', (tester) async {
      // Stream.multi with a replayed value — the shape the engine actually
      // hands over (replayLatest in sync_adapter.dart). It matters here: the
      // detail screen listens to the SAME stream object the feed did, and a
      // single-subscription or plain broadcast stream leaves it spinning.
      final events = Stream<List<OrderEventRow>>.multi(
        (c) => c.add([
          OrderEventRow(
            id: 'e2',
            orderId: '983979e8-4004-48b5-b397-8cd73442f45c',
            status: 'shipped',
            previousStatus: 'paid',
            createdAt: DateTime.utc(2026, 9, 23, 16, 40),
          ),
        ]),
      );

      await tester.pumpWidget(
        MaterialApp(
          home: HistoryScreen(
            events: events,
            store: store,
            uploadRuns: (rows) async {},
            runSuite: () async {},
          ),
        ),
      );
      await _settle(tester);

      await tester.tap(find.byKey(const Key('history-event-e2')));
      await _settle(tester);
      expect(find.byKey(const Key('history-detail-screen')), findsOneWidget);
      // The payload the smoke test reads, on screen with its routing keys.
      expect(find.textContaining('"cairn_route": "/history/e2"'), findsOneWidget);
    });

    testWidgets('the bench is off the feed, one tap away in the app bar', (
      tester,
    ) async {
      await tester.pumpWidget(
        MaterialApp(
          home: HistoryScreen(
            events: Stream.value(const []),
            store: store,
            uploadRuns: (rows) async {},
            runSuite: () async {},
          ),
        ),
      );
      await _settle(tester);
      // The regression this file exists to catch (user request 2026-09-23):
      // the eval banner and the run/upload buttons were ON the History tab.
      expect(find.byKey(const Key('bench-eval-banner')), findsNothing);
      expect(find.byKey(const Key('run-suite-button')), findsNothing);

      await tester.tap(find.byKey(const Key('open-bench-button')));
      await _settle(tester);
      expect(find.byKey(const Key('bench-screen')), findsOneWidget);
      expect(find.byKey(const Key('bench-eval-banner')), findsOneWidget);
    });
  });
}
