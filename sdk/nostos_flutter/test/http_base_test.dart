// Pin for the REST-base derivation (`NostosDatabase.deriveHttpBase`): the
// push-token registration and schema fetch ride the SAME base as /sync, so
// a sync URL that reaches the server through a path-prefixed reverse proxy
// (the arxa engine tunnel route `/__nostos/sync`, B2 phase-1b) must keep its
// prefix on `/<prefix>/schema` and `/<prefix>/push-tokens`. A root-level
// `/sync` — every normal self-host bind — keeps the host+port-only base.
// Pure-Dart (no native library).

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('root-level /sync keeps the historical host+port base', () {
    expect(
      NostosDatabase.deriveHttpBase('ws://127.0.0.1:8190/sync'),
      'http://127.0.0.1:8190',
    );
  });

  test('wss maps to https, default port omitted', () {
    expect(NostosDatabase.deriveHttpBase('wss://nostos.example.com/sync'),
        'https://nostos.example.com');
  });

  test('a path-prefixed sync URL keeps its prefix directory', () {
    expect(
      NostosDatabase.deriveHttpBase('ws://127.0.0.1:8765/__nostos/sync'),
      'http://127.0.0.1:8765/__nostos',
    );
  });

  test('deeper prefixes keep every segment', () {
    expect(
      NostosDatabase.deriveHttpBase('ws://10.0.0.2:80/rail/phone/sync'),
      'http://10.0.0.2:80/rail/phone',
    );
  });

  test('a bare /sync/ trailing slash still lands at the root', () {
    expect(NostosDatabase.deriveHttpBase('ws://h.example/sync/'), 'http://h.example');
  });

  test('port zero is omitted, not emitted', () {
    expect(NostosDatabase.deriveHttpBase('ws://127.0.0.1:0/sync'), 'http://127.0.0.1');
  });
}
