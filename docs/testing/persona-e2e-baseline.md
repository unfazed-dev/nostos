# Persona-driven E2E baseline for Flutter fixtures

The reference implementation is `fixtures/flutter/pomodoro/`. To adopt the
convention in any Flutter fixture (or SDK example app):

1. **Personas are testable specs.** `docs/personas/<slug>.md` per persona:
   profile, goals, frustrations, a Journey table (step → expected state), and
   Invariants. Personas describe *behavioral archetypes* that stress different
   state-machine paths — not marketing demographics.
2. **1:1 journey binding.** Each persona gets
   `integration_test/journeys/<slug>_journey_test.dart`; the doc's Journey
   rows appear as ordered comments in the test. A `persona_mapping_test.dart`
   guard (copy from the pomodoro fixture) fails the unit suite on any drift.
3. **Compressed time is a product config, not a test hack.** Ship a
   `demo()` config (seconds, not minutes) reachable by real users
   (`--dart-define=DEMO_MODE=true`), and keep a unit test proving the state
   machine's transition graph is identical across configs.
4. **Assert transitions, never wall-clock.** Journeys wait by polling keyed
   widgets (`waitForText`) and assert phase/state/count changes. Any test
   asserting elapsed duration is a review-blocker.
5. **The ladder** (cheapest first): unit (ports mocked with mocktail) →
   widget → smoke (`integration_test/smoke_test.dart`: real `main()`, first
   frame, keyed widgets present) → persona journeys (`-d macos`) → Patrol,
   only once the app has native surfaces (permissions, notifications) —
   patrol_cli is installed and the escalation is additive. (Run each
   integration file in its own `flutter test` invocation on desktop — see the
   pomodoro Makefile's `fixture-e2e` for the loop pattern; Flutter desktop
   can't aggregate-launch multiple files against one app bundle.)
6. **Keys, not text.** Every asserted widget has a stable `Key('area.thing')`;
   copy changes must not break journeys.

When the nostos Flutter SDK lands (docs/plans/flutter-sdk.md), fixtures gain a
sync layer; persona journeys then double as SDK E2E: same personas, plus sync
assertions (offline write → reconnect → row echoed).
