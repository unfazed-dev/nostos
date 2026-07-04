# User Personas — testable specs

Each persona in this directory is an executable specification: one markdown
file describing who the user is and how they behave, bound 1:1 to an E2E
journey in `integration_test/journeys/` that walks their exact behavior and
asserts the state transitions they'd observe.

Contract (enforced by `test/persona_mapping_test.dart`):
- persona doc `docs/personas/<slug>.md` (kebab-case slug)
- journey test `integration_test/journeys/<slug with _>_journey_test.dart`
- every persona doc section "Journey" is a table whose Step rows appear as
  comments in the journey test, in order — reviewers diff doc against test.

All journeys run in compressed time (`TimerConfig.demo()` or a per-persona
config) — a real app run mode, proven equivalent to real durations by the
equivalence test in `test/viewmodels/pomodoro_viewmodel_test.dart`.
Journeys assert phase/sessions transitions, never wall-clock elapsed time.

The repo-wide convention this instantiates: `docs/testing/persona-e2e-baseline.md`.
