import 'dart:async';

import 'package:atlet/connectivity_guard.dart';
import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('offline event surfaces online=false, regain surfaces true', () async {
    final events = StreamController<List<ConnectivityResult>>();
    final seen = <bool>[];
    final guard = ConnectivityGuard(
      events: events.stream,
      onOnlineChanged: (online) async => seen.add(online),
    )..start();

    events.add(const [ConnectivityResult.none]);
    events.add(const [ConnectivityResult.wifi]);
    await Future<void>.delayed(Duration.zero);

    expect(seen, [false, true]);
    await guard.dispose();
    await events.close();
  });

  test('dedupes repeated identical states (foreground re-reports)', () async {
    // _lastOnline seeds true, so lead with an offline transition to create
    // a real state change, then exercise dedup on both sides of it.
    final events = StreamController<List<ConnectivityResult>>();
    final seen = <bool>[];
    final guard = ConnectivityGuard(
      events: events.stream,
      onOnlineChanged: (online) async => seen.add(online),
    )..start();

    events.add(const [ConnectivityResult.none]);
    events.add(const [ConnectivityResult.none]); // dup — no callback
    events.add(const [ConnectivityResult.wifi]);
    events.add(const [ConnectivityResult.wifi, ConnectivityResult.mobile]); // dup
    await Future<void>.delayed(Duration.zero);

    expect(seen, [false, true]);
    await guard.dispose();
    await events.close();
  });

  test('mixed results containing none but also a real transport = online',
      () async {
    // connectivity_plus can report e.g. [vpn, wifi]; only an exclusive
    // "none" means offline. _lastOnline seeds true, so lead offline first
    // to create a state change, then verify the mixed bag reads online.
    final events = StreamController<List<ConnectivityResult>>();
    final seen = <bool>[];
    final guard = ConnectivityGuard(
      events: events.stream,
      onOnlineChanged: (online) async => seen.add(online),
    )..start();

    events.add(const [ConnectivityResult.none]); // → offline (change from seed)
    events.add(const [ConnectivityResult.none, ConnectivityResult.wifi]); // → online
    await Future<void>.delayed(Duration.zero);

    expect(seen, [false, true]);
    await guard.dispose();
    await events.close();
  });
}
