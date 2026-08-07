import 'dart:async';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:flutter_test/flutter_test.dart';

/// Regression: adapters feed `db.watch(...)` into broadcast controllers,
/// which do not replay. Read-only tables (`products`) emit exactly one
/// snapshot at init(), so a ShopScreen built after an engine switch
/// subscribed too late and spun on its loading state forever. replayLatest
/// must hand late subscribers the most recent value, then live events.
void main() {
  test('late subscriber receives the cached latest value, then live events',
      () async {
    final controller = StreamController<List<int>>.broadcast();
    List<int>? last;
    // Simulate init(): the adapter's own central subscription caches values.
    controller.stream.listen((v) => last = v);

    // The one-and-only snapshot fires before any UI subscriber exists.
    controller.add([1, 2, 3]);
    await Future<void>.delayed(Duration.zero);

    // Late subscriber (ShopScreen after an engine switch).
    final seen = <List<int>>[];
    final sub =
        replayLatest(controller.stream, () => last).listen(seen.add);
    await Future<void>.delayed(Duration.zero);
    expect(seen, [
      [1, 2, 3]
    ], reason: 'late subscriber must get the replayed snapshot');

    // Live events still flow after the replay.
    controller.add([4]);
    await Future<void>.delayed(Duration.zero);
    expect(seen.last, [4]);

    await sub.cancel();
    await controller.close();
  });

  test('no cached value yet: subscriber just waits for live events',
      () async {
    final controller = StreamController<int>.broadcast();
    final seen = <int>[];
    final sub =
        replayLatest<int>(controller.stream, () => null).listen(seen.add);
    await Future<void>.delayed(Duration.zero);
    expect(seen, isEmpty);

    controller.add(7);
    await Future<void>.delayed(Duration.zero);
    expect(seen, [7]);

    await sub.cancel();
    await controller.close();
  });

  test('cancelling the wrapped stream detaches from the source', () async {
    final controller = StreamController<int>.broadcast();
    final sub = replayLatest(controller.stream, () => 0).listen((_) {});
    await Future<void>.delayed(Duration.zero);
    expect(controller.hasListener, isTrue);
    await sub.cancel();
    expect(controller.hasListener, isFalse);
    await controller.close();
  });
}
