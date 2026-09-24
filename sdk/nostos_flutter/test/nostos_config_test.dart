// ADR-0046: `NostosConfig.load` reads the pre-rename asset when only that one
// is bundled. Pure-Dart (no native library, no asset bundle).

import 'package:nostos_flutter/src/nostos_config.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';

Future<String> Function(String) bundle(Map<String, String> assets) =>
    (name) async => assets[name] ?? (throw FlutterError('missing $name'));

void main() {
  test('legacy only: reads legacy', () async {
    final load = bundle({'old.json': 'old'});
    expect(await loadAssetOrLegacy(load, 'new.json', 'old.json'), 'old');
  });

  test('primary only: reads primary', () async {
    final load = bundle({'new.json': 'new'});
    expect(await loadAssetOrLegacy(load, 'new.json', 'old.json'), 'new');
  });

  test('both: primary wins', () async {
    final load = bundle({'new.json': 'new', 'old.json': 'old'});
    expect(await loadAssetOrLegacy(load, 'new.json', 'old.json'), 'new');
  });

  test('neither: the missing-asset error propagates', () {
    expect(
      loadAssetOrLegacy(bundle({}), 'new.json', 'old.json'),
      throwsA(isA<FlutterError>()),
    );
  });

  test('equal names: one read, no fallback', () async {
    var reads = 0;
    Future<String> load(String name) async {
      reads++;
      throw FlutterError('missing $name');
    }

    await expectLater(
      loadAssetOrLegacy(load, 'new.json', 'new.json'),
      throwsA(isA<FlutterError>()),
    );
    expect(reads, 1);
  });
}
