// Widget tests for lib/ui/home.dart + lib/ui/detail.dart (task-12). These
// pump against a fake SyncAdapter — no device/emulator available in this
// environment, so the airplane-mode smoke task-12-brief asks for is not run
// here (see task-12-report.md). What these tests DO prove: the UI renders
// exclusively from watchSessions() emissions — a write is invisible until
// the adapter echoes it back on the stream, never from local state.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/ui/detail.dart';
import 'package:atlet/ui/home.dart';

/// Minimal fake: addSession/deleteSession mutate an in-memory map but only
/// push to the stream when [flush] is called. That gap is the point — it
/// lets tests assert the UI shows nothing new until the stream actually
/// emits, i.e. there is no local cache/optimistic render path.
class _FakeAdapter implements SyncAdapter {
  final _sessions = <String, SessionRow>{};
  final _controller = StreamController<List<SessionRow>>.broadcast();

  @override
  String get engine => 'fake';

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {}

  @override
  Future<void> signOut() async {
    _sessions.clear();
  }

  @override
  Future<String> addSession(SessionRow s) async {
    _sessions[s.id] = s;
    return s.id;
  }

  @override
  Future<void> deleteSession(String id) async {
    _sessions.remove(id);
  }

  @override
  Stream<List<SessionRow>> watchSessions() => _controller.stream;

  @override
  Stream<List<ProductRow>> watchProducts() => const Stream.empty();

  @override
  Stream<bool> get connected => const Stream.empty();

  @override
  Future<void> setConnected(bool up) async {}

  @override
  Stream<SyncMark> get marks => const Stream.empty();

  /// Test-only: push the current in-memory state onto the watch stream —
  /// this is the only way rows become visible to the UI.
  void flush() => _controller.add(_sessions.values.toList());

  void dispose() => _controller.close();
}

SessionRow _fixture({String id = 'w1', int streak = 4}) => SessionRow(
      id: id,
      title: 'Sunrise 5k',
      type: 'distance',
      metric: 5,
      unit: 'km',
      streak: streak,
      occurredOn: DateTime(2026, 8, 1),
    );

void main() {
  testWidgets('session list renders only after watchSessions emits', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);

    await tester.pumpWidget(MaterialApp(home: TrainingHome(adapter: adapter)));
    await tester.pump();
    expect(find.text('Sunrise 5k'), findsNothing);

    await adapter.addSession(_fixture());
    await tester.pump();
    // Written through the adapter, but not yet emitted on the stream.
    expect(find.text('Sunrise 5k'), findsNothing);

    adapter.flush();
    await tester.pumpAndSettle();
    expect(find.text('Sunrise 5k'), findsOneWidget);
    expect(find.text('Distance · 5 km'), findsOneWidget);
    expect(find.byKey(const Key('streak-chip')), findsOneWidget);
  });

  testWidgets('add-session sheet writes through the adapter and renders on echo', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);

    await tester.pumpWidget(MaterialApp(home: TrainingHome(adapter: adapter)));
    adapter.flush();
    await tester.pump();

    await tester.tap(find.byKey(const Key('add-session-button')));
    await tester.pumpAndSettle();

    await tester.enterText(find.byKey(const Key('session-title-field')), 'Tabata Burnout');
    await tester.enterText(find.byKey(const Key('session-metric-field')), '4');
    // Rebuild so the Save button's onPressed picks up the now-valid form
    // state (onChanged's setState doesn't rebuild until the next pump).
    await tester.pump();
    await tester.tap(find.byKey(const Key('save-session-button')));
    await tester.pumpAndSettle();

    // Sheet closed, write is in flight, but nothing renders until the
    // stream echoes it back.
    expect(find.text('Tabata Burnout'), findsNothing);

    adapter.flush();
    await tester.pumpAndSettle();
    expect(find.text('Tabata Burnout'), findsOneWidget);
  });

  testWidgets('detail: complete removes the session and pops', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);
    await adapter.addSession(_fixture());

    // SessionDetail must be pushed (not the route's `home:`) for this test to
    // actually exercise the pop path — canPop() is always false at the root,
    // so a `home:`-only setup would let the pop code silently never run.
    await tester.pumpWidget(MaterialApp(
      home: Builder(
        builder: (context) => Scaffold(
          body: Center(
            child: TextButton(
              onPressed: () => Navigator.of(context).push(
                MaterialPageRoute<void>(
                  builder: (_) => SessionDetail(adapter: adapter, sessionId: 'w1'),
                ),
              ),
              child: const Text('open detail'),
            ),
          ),
        ),
      ),
    ));

    await tester.tap(find.text('open detail'));
    await tester.pumpAndSettle();
    adapter.flush();
    await tester.pumpAndSettle();
    expect(find.text('Sunrise 5k'), findsOneWidget);

    await tester.tap(find.byKey(const Key('complete-session-button')));
    await tester.pumpAndSettle();
    adapter.flush();
    await tester.pumpAndSettle();

    expect(adapter._sessions.containsKey('w1'), isFalse);
    // Popped back to the placeholder screen.
    expect(find.text('open detail'), findsOneWidget);
    expect(find.text('Sunrise 5k'), findsNothing);
  });

  testWidgets('detail: delete asks for confirmation before removing', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);
    await adapter.addSession(_fixture());

    await tester.pumpWidget(MaterialApp(
      home: SessionDetail(adapter: adapter, sessionId: 'w1'),
    ));
    adapter.flush();
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('delete-session-button')));
    await tester.pumpAndSettle();
    expect(find.text("This can't be undone."), findsOneWidget);

    // Cancel leaves the row untouched.
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(adapter._sessions.containsKey('w1'), isTrue);

    await tester.tap(find.byKey(const Key('delete-session-button')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-delete-button')));
    await tester.pumpAndSettle();
    adapter.flush();
    await tester.pumpAndSettle();

    expect(adapter._sessions.containsKey('w1'), isFalse);
  });
}
