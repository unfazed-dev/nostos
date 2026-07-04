import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:mocktail/mocktail.dart';
import 'package:pomodoro/domain/ticker.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

class _MockTicker extends Mock implements Ticker {}

void main() {
  late _MockTicker ticker;
  late StreamController<void> ticks;

  setUp(() {
    ticker = _MockTicker();
    ticks = StreamController<void>.broadcast(sync: true);
    when(() => ticker.ticks()).thenAnswer((_) => ticks.stream);
  });

  tearDown(() => ticks.close());

  Future<void> pumpApp(WidgetTester t) =>
      t.pumpWidget(PomodoroApp(config: const TimerConfig.demo(), ticker: ticker));

  void tick([int n = 1]) {
    for (var i = 0; i < n; i++) {
      ticks.add(null);
    }
  }

  // The Key is attached to the Text widget itself, so we read the Text.data
  // straight off the keyed widget. This both pins the key AND the rendered text.
  String textOf(WidgetTester t, Key key) =>
      t.widget<Text>(find.byKey(key)).data ?? '';

  const displayKey = Key('timer.display');
  const phaseKey = Key('timer.phase');
  const sessionsKey = Key('timer.sessions');

  testWidgets('renders initial timer, phase label, and zero sessions', (t) async {
    await pumpApp(t);

    expect(find.byKey(displayKey), findsOneWidget);
    expect(textOf(t, displayKey), '00:03');

    expect(find.byKey(phaseKey), findsOneWidget);
    expect(textOf(t, phaseKey), 'Focus');

    expect(find.byKey(sessionsKey), findsOneWidget);
    expect(textOf(t, sessionsKey), '0');
  });

  testWidgets('tapping start begins countdown; display updates per tick', (t) async {
    await pumpApp(t);

    await t.tap(find.byKey(const Key('timer.start')));
    await t.pump();

    tick();
    await t.pump();

    expect(textOf(t, displayKey), '00:02');
  });

  testWidgets('completing a work phase updates phase label and session count', (t) async {
    await pumpApp(t);

    await t.tap(find.byKey(const Key('timer.start')));
    await t.pump();

    tick(3);
    await t.pump();

    expect(textOf(t, phaseKey), 'Short break');
    expect(textOf(t, sessionsKey), '1');
  });

  testWidgets('reset returns the UI to the initial state', (t) async {
    await pumpApp(t);

    await t.tap(find.byKey(const Key('timer.start')));
    await t.pump();

    tick(3);
    await t.pump();

    // Sanity: we advanced past the initial state before resetting.
    expect(textOf(t, sessionsKey), '1');

    await t.tap(find.byKey(const Key('timer.reset')));
    await t.pump();

    expect(textOf(t, displayKey), '00:03');
    expect(textOf(t, phaseKey), 'Focus');
    expect(textOf(t, sessionsKey), '0');
  });
}
