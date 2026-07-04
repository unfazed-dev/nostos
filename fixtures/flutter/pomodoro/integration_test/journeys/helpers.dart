import 'dart:async';

import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pomodoro/domain/ticker.dart';

/// A [Ticker] the test drives by hand. Emits a tick ONLY when the test calls
/// [tick]; never on a wall-clock interval. This is the seam the Ticker port
/// exists for: journeys advance the state machine deterministically by emitting
/// the exact number of ticks a phase needs (e.g. `config.work.inSeconds`) and
/// pumping once, then assert immediately — no polling, no wall-clock, no drift.
///
/// `sync: true` so each `tick()` delivers to the ViewModel synchronously before
/// it returns; the follow-up `tester.pump()` then rebuilds the widget tree with
/// the new state. This is identical to the unit/widget-test harness and is what
/// makes the journeys 100% reliable on the macOS desktop target (where a real
/// SystemTicker drifts when the launched .app is throttled).
class FakeTicker implements Ticker {
  FakeTicker() : _controller = StreamController<void>(sync: true);

  final StreamController<void> _controller;

  /// Emit [n] ticks (default 1) into the stream. Each tick is delivered to the
  /// ViewModel synchronously; pair with `await tester.pump()` to rebuild.
  void tick([int n = 1]) {
    for (var i = 0; i < n; i++) {
      _controller.add(null);
    }
  }

  @override
  Stream<void> ticks() => _controller.stream;

  void dispose() => _controller.close();
}

/// Read the rendered text of the widget keyed by [key].
String textOf(WidgetTester tester, Key key) =>
    tester.widget<Text>(find.byKey(key)).data ?? '';

/// Tap a keyed IconButton and pump once so the onPressed side-effect settles.
Future<void> tapKey(WidgetTester tester, String key) async {
  await tester.tap(find.byKey(Key(key)));
  await tester.pump();
}
