import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

/// Advisor-mandated drift guard: every persona doc has a journey, and every
/// journey has a persona doc. Fails the plain `flutter test` run — no CI
/// wiring needed.
void main() {
  test('persona docs and journey tests are 1:1', () {
    final personas = Directory('docs/personas')
        .listSync()
        .whereType<File>()
        .map((f) => f.uri.pathSegments.last)
        .where((n) => n.endsWith('.md') && n != 'README.md')
        .map((n) => n.replaceAll('.md', '').replaceAll('-', '_'))
        .toSet();
    final journeys = Directory('integration_test/journeys')
        .listSync()
        .whereType<File>()
        .map((f) => f.uri.pathSegments.last)
        .where((n) => n.endsWith('_journey_test.dart'))
        .map((n) => n.replaceAll('_journey_test.dart', ''))
        .toSet();
    expect(journeys, personas,
        reason: 'each docs/personas/<slug>.md needs '
            'integration_test/journeys/<slug>_journey_test.dart and vice versa');
  });
}
