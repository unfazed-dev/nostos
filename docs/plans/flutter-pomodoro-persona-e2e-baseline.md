# Flutter Fixtures — Persona-Driven Smoke + E2E Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add two Flutter apps to nostos as **test fixtures**: (Part I, Tasks 1–7) a pomodoro app with documented user personas mapping 1:1 to smoke + E2E journeys — the reusable persona→E2E convention every future Flutter fixture follows; (Part II, Tasks 8–10) a **todo app with Supabase cloud + authentication**, whose dual-mode smoke test runs fully mocked today and flips to live Supabase the moment the operator provides API keys. The Phase-G Flutter SDK plan retrofits real nostos sync into both.

**Architecture:** Plain `ChangeNotifier` ViewModel + a domain `Ticker` port (the clock seam — unit tests drive time by hand, no real waits). Compressed-time `TimerConfig.demo()` is a real user-facing demo mode (`--dart-define=DEMO_MODE=true`), which is also what makes E2E persona journeys run in seconds. E2E via Flutter's official `integration_test` on the macOS desktop target; Patrol is the documented native-escalation path (patrol_cli 4.4.0 is installed) but is not needed while the fixture has no native surfaces.

**Tech Stack:** Flutter 3.44.0 stable (installed via fvm at `~/fvm/default/bin/flutter`), `flutter_test`, `integration_test` (SDK), `mocktail` (only added dependency). No state-management or DI packages — YAGNI for a fixture.

## Global Constraints

- **Everything lives inside the nostos repo.** Fixture root: `fixtures/flutter/pomodoro/`. Nothing is created outside the nostos tree.
- Flutter `3.44.0` stable (fvm default); Dart SDK constraint as generated (`^3.12.0`).
- Only new dependency: `mocktail ^1.0.4` (dev). `integration_test` comes from the SDK.
- Nostos's Rust gate is untouched: `make ci` stays Rust-only. Flutter fixture verbs are separate Make targets (Task 7).
- Widget keys use the `timer.*` naming convention (`timer.display`, `timer.phase`, `timer.sessions`, `timer.start`, `timer.reset`, `timer.skip`) — E2E finds by key, never by text-that-might-restyle.
- E2E asserts **state transitions, never wall-clock elapsed time** (advisor: exact-duration assertions are flaky under CI load).
- Compressed time is config, not test hackery: the state machine's transition graph must be provably independent of literal durations (equivalence test, Task 2).
- Commits: single line, conventional prefix, no author mentions.
- Persona docs and journey files are bound by a guard test (Task 4): a persona without a journey — or vice versa — fails the suite.

---

## Why this fixture exists (context for the executor)

1. **Baseline convention.** Nostos's roadmap ships Flutter/RN SDKs (master plan Part VIII). Every SDK needs example apps with real E2E coverage. This fixture establishes the pattern — persona docs as testable specs, smoke boot test, compressed-time journeys — before any SDK code exists, on an app small enough to hold in one context.
2. **Future SDK demo.** A pomodoro app with synced session history is a natural nostos demo (offline writes → sync). The `docs/plans/flutter-sdk.md` plan (gated on v0.1) retrofits nostos sync into this fixture; the persona journeys then double as SDK E2E tests.
3. **The v0.1 stranger test** (master plan Task F1) needs an offline todo app built by a stranger; this fixture's convention doc is the template that makes future fixture apps cheap.

Verified toolchain facts (2026-07-04): Flutter 3.44.0 stable via fvm; `patrol_cli 4.4.0` and `stacked_cli 1.15.5` on PATH; probe-runner skill available for boot-smoke screenshots; Xcode + macOS desktop target functional on this machine.

---

### Task 1: Fixture scaffold + domain layer

**Files:**
- Create: `fixtures/flutter/pomodoro/` (via `flutter create`)
- Create: `fixtures/flutter/pomodoro/lib/domain/timer_config.dart`
- Create: `fixtures/flutter/pomodoro/lib/domain/ticker.dart`
- Modify: `fixtures/flutter/pomodoro/pubspec.yaml`

**Interfaces:**
- Produces: `TimerConfig` (`work`, `shortBreak`, `longBreak`, `cyclesPerLongBreak`, `autoAdvance`, plus `const TimerConfig.demo()`) and `Ticker`/`SystemTicker` — every later task consumes these exact names.

- [x] **Step 1: Scaffold** (from the nostos repo root; `--empty` keeps main.dart minimal):

```bash
cd /Volumes/developer_ssd/Developer/nostos
mkdir -p fixtures/flutter
flutter create --project-name pomodoro --org run.nostos --platforms macos,ios,android --empty fixtures/flutter/pomodoro
```

- [x] **Step 2: pubspec** — replace the generated `dev_dependencies`/description so the full file reads:

```yaml
name: pomodoro
description: "Nostos Flutter fixture: persona-driven smoke + E2E baseline (docs/testing/persona-e2e-baseline.md)."
publish_to: 'none'
version: 0.1.0+1

environment:
  sdk: ^3.12.0

dependencies:
  flutter:
    sdk: flutter

dev_dependencies:
  flutter_test:
    sdk: flutter
  integration_test:
    sdk: flutter
  flutter_lints: ^6.0.0
  mocktail: ^1.0.4

flutter:
  uses-material-design: true
```

- [x] **Step 3: `lib/domain/timer_config.dart`:**

```dart
/// Phase durations and cycle policy for the pomodoro machine.
///
/// `demo()` is a real, user-facing configuration (run the app with
/// `--dart-define=DEMO_MODE=true`), not a test-only hack: compressed phases
/// make demos and E2E persona journeys run in seconds while exercising the
/// exact same state machine. The transition graph depends only on this config,
/// never on literal durations — the equivalence test in
/// pomodoro_viewmodel_test.dart proves it.
class TimerConfig {
  const TimerConfig({
    this.work = const Duration(minutes: 25),
    this.shortBreak = const Duration(minutes: 5),
    this.longBreak = const Duration(minutes: 15),
    this.cyclesPerLongBreak = 4,
    this.autoAdvance = false,
  });

  /// Compressed phases for demos and E2E journeys.
  const TimerConfig.demo()
      : this(
          work: const Duration(seconds: 3),
          shortBreak: const Duration(seconds: 2),
          longBreak: const Duration(seconds: 4),
          cyclesPerLongBreak: 2,
        );

  final Duration work;
  final Duration shortBreak;
  final Duration longBreak;

  /// A long break replaces the short one after this many completed work phases.
  final int cyclesPerLongBreak;

  /// When true, the next phase starts ticking immediately on transition.
  /// Default false: the classic pomodoro asks the user to start each phase.
  final bool autoAdvance;
}
```

- [x] **Step 4: `lib/domain/ticker.dart`:**

```dart
/// The clock port. The ViewModel consumes ticks through this seam so unit
/// tests can drive time by hand (mocktail + a StreamController) and never
/// wait on a real clock.
abstract interface class Ticker {
  /// Emits once per second while listened to.
  Stream<void> ticks();
}

/// Production clock: one tick per wall-clock second.
class SystemTicker implements Ticker {
  @override
  Stream<void> ticks() => Stream<void>.periodic(const Duration(seconds: 1));
}
```

- [x] **Step 5: Verify** — `cd fixtures/flutter/pomodoro && flutter pub get && flutter analyze` → no issues.
- [x] **Step 6: Commit** — `git add fixtures/flutter/pomodoro && git commit -m "feat: scaffold flutter pomodoro fixture with TimerConfig and Ticker port"`

### Task 2: ViewModel — TDD with the advisor's regression guards

**Files:**
- Create: `fixtures/flutter/pomodoro/lib/viewmodels/pomodoro_viewmodel.dart`
- Test: `fixtures/flutter/pomodoro/test/viewmodels/pomodoro_viewmodel_test.dart`

**Interfaces:**
- Consumes: `Ticker`, `TimerConfig` (Task 1).
- Produces: `Phase { work, shortBreak, longBreak }` and `PomodoroViewModel` with `phase`, `remaining` (Duration), `running` (bool), `completedWork` (int), `start()`, `pause()`, `reset()`, `skip()`, `onLifecycle(AppLifecycleState)` — Tasks 3, 5, 6 rely on these exact names.

- [x] **Step 1: Write the failing tests** (`test/viewmodels/pomodoro_viewmodel_test.dart`):

```dart
import 'dart:async';

import 'package:flutter/widgets.dart' show AppLifecycleState;
import 'package:flutter_test/flutter_test.dart';
import 'package:mocktail/mocktail.dart';
import 'package:pomodoro/domain/ticker.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/viewmodels/pomodoro_viewmodel.dart';

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

  PomodoroViewModel vm({TimerConfig config = const TimerConfig.demo()}) =>
      PomodoroViewModel(ticker: ticker, config: config);

  void tick([int n = 1]) {
    for (var i = 0; i < n; i++) {
      ticks.add(null);
    }
  }

  test('initial state: work phase, full duration, stopped, zero sessions', () {
    final m = vm();
    expect(m.phase, Phase.work);
    expect(m.remaining, const TimerConfig.demo().work);
    expect(m.running, isFalse);
    expect(m.completedWork, 0);
  });

  test('start then ticks decrement remaining', () {
    final m = vm()..start();
    tick(2);
    expect(m.remaining, const Duration(seconds: 1));
    expect(m.running, isTrue);
  });

  test('pause freezes remaining; ticks while paused are ignored', () {
    final m = vm()..start();
    tick();
    m.pause();
    tick(5);
    expect(m.remaining, const Duration(seconds: 2));
    expect(m.running, isFalse);
  });

  test('rapid re-entry start-pause-start never double-fires (single subscription)', () {
    final m = vm()..start();
    m.pause();
    m.start();
    m.pause();
    m.start();
    tick(); // exactly one tick must decrement exactly one second
    expect(m.remaining, const Duration(seconds: 2));
    verify(() => ticker.ticks()).called(1); // subscription created once, ever
  });

  test('work completion: session credited, short break queued, stopped (autoAdvance off)', () {
    final m = vm()..start();
    tick(3);
    expect(m.completedWork, 1);
    expect(m.phase, Phase.shortBreak);
    expect(m.remaining, const TimerConfig.demo().shortBreak);
    expect(m.running, isFalse);
  });

  test('long break after cyclesPerLongBreak work phases', () {
    final m = vm()..start();
    tick(3); // work #1 done -> shortBreak
    m.start();
    tick(2); // break done -> work
    m.start();
    tick(3); // work #2 done -> demo config has cyclesPerLongBreak = 2
    expect(m.completedWork, 2);
    expect(m.phase, Phase.longBreak);
    expect(m.remaining, const TimerConfig.demo().longBreak);
  });

  test('break completion returns to work at full duration', () {
    final m = vm()..start();
    tick(3);
    m.start();
    tick(2);
    expect(m.phase, Phase.work);
    expect(m.remaining, const TimerConfig.demo().work);
  });

  test('reset restores initial state from any point', () {
    final m = vm()..start();
    tick(3); // one full work phase
    m.start();
    tick(1); // mid-break
    m.reset();
    expect(m.phase, Phase.work);
    expect(m.remaining, const TimerConfig.demo().work);
    expect(m.running, isFalse);
    expect(m.completedWork, 0);
  });

  test('skip advances the phase WITHOUT crediting a work session', () {
    final m = vm()..start();
    tick(1);
    m.skip();
    expect(m.phase, Phase.shortBreak);
    expect(m.completedWork, 0);
  });

  test('lifecycle: backgrounding auto-pauses a running timer', () {
    final m = vm()..start();
    m.onLifecycle(AppLifecycleState.paused);
    expect(m.running, isFalse);
    tick(5);
    expect(m.remaining, const TimerConfig.demo().work); // frozen
  });

  test('equivalence: transition graph is identical across configs (compressed time is honest)', () {
    // Two configs with different durations but the same cycle policy must
    // produce the same (phase, completedWork) sequence when each phase is
    // driven to completion. This is what licenses demo-config E2E journeys.
    List<(Phase, int)> run(TimerConfig c) {
      final m = vm(config: c);
      final seen = <(Phase, int)>[];
      Duration current() => switch (m.phase) {
            Phase.work => c.work,
            Phase.shortBreak => c.shortBreak,
            Phase.longBreak => c.longBreak,
          };
      for (var i = 0; i < 5; i++) {
        m.start();
        tick(current().inSeconds);
        seen.add((m.phase, m.completedWork));
      }
      return seen;
    }

    final a = run(const TimerConfig(
        work: Duration(seconds: 3),
        shortBreak: Duration(seconds: 2),
        longBreak: Duration(seconds: 4),
        cyclesPerLongBreak: 2));
    final b = run(const TimerConfig(
        work: Duration(seconds: 7),
        shortBreak: Duration(seconds: 5),
        longBreak: Duration(seconds: 9),
        cyclesPerLongBreak: 2));
    expect(a, b);
  });
}
```

- [x] **Step 2: Run — expect FAIL** (`PomodoroViewModel` undefined): `flutter test test/viewmodels/`
- [x] **Step 3: Implement `lib/viewmodels/pomodoro_viewmodel.dart`:**

```dart
import 'dart:async';

import 'package:flutter/widgets.dart';

import '../domain/ticker.dart';
import '../domain/timer_config.dart';

enum Phase { work, shortBreak, longBreak }

/// The pomodoro state machine. All timing flows through the [Ticker] port;
/// the machine itself never touches a real clock.
class PomodoroViewModel extends ChangeNotifier {
  PomodoroViewModel({required Ticker ticker, this.config = const TimerConfig()})
      : _ticker = ticker,
        _remaining = config.work;

  final TimerConfig config;
  final Ticker _ticker;
  StreamSubscription<void>? _sub;

  Phase _phase = Phase.work;
  Duration _remaining;
  bool _running = false;
  int _completedWork = 0;

  Phase get phase => _phase;
  Duration get remaining => _remaining;
  bool get running => _running;
  int get completedWork => _completedWork;

  void start() {
    if (_running) return;
    _running = true;
    // Single subscription for the VM's lifetime: double-start cannot
    // double-fire; pause gates ticks instead of tearing down the stream.
    _sub ??= _ticker.ticks().listen((_) => _onTick());
    notifyListeners();
  }

  void pause() {
    if (!_running) return;
    _running = false;
    notifyListeners();
  }

  void reset() {
    _running = false;
    _phase = Phase.work;
    _completedWork = 0;
    _remaining = config.work;
    notifyListeners();
  }

  /// Advance to the next phase without crediting a completed work session.
  void skip() {
    _transition(credit: false);
  }

  /// Auto-pause when the app leaves the foreground.
  // ponytail: pause-on-background; wall-clock catch-up when mobile
  // background continuation matters (the future nostos-sdk retrofit).
  void onLifecycle(AppLifecycleState state) {
    if (state != AppLifecycleState.resumed) pause();
  }

  void _onTick() {
    if (!_running) return;
    _remaining -= const Duration(seconds: 1);
    if (_remaining <= Duration.zero) {
      _transition(credit: _phase == Phase.work);
    } else {
      notifyListeners();
    }
  }

  void _transition({required bool credit}) {
    if (_phase == Phase.work) {
      if (credit) _completedWork++;
      _phase = (_completedWork > 0 && _completedWork % config.cyclesPerLongBreak == 0)
          ? Phase.longBreak
          : Phase.shortBreak;
    } else {
      _phase = Phase.work;
    }
    _remaining = switch (_phase) {
      Phase.work => config.work,
      Phase.shortBreak => config.shortBreak,
      Phase.longBreak => config.longBreak,
    };
    _running = config.autoAdvance;
    notifyListeners();
  }

  @override
  void dispose() {
    _sub?.cancel();
    super.dispose();
  }
}
```

Note one subtlety the tests pin down: `skip()` from a work phase with zero completed sessions must go to `shortBreak`, not `longBreak` — hence the `_completedWork > 0` guard.

- [x] **Step 4: Run — expect PASS**: `flutter test test/viewmodels/` (11 tests green).
- [x] **Step 5: Commit** — `git commit -m "feat: pomodoro viewmodel with ticker port, lifecycle auto-pause, and equivalence-proven transitions"`

### Task 3: View + widget tests

**Files:**
- Create: `fixtures/flutter/pomodoro/lib/views/pomodoro_view.dart`
- Modify: `fixtures/flutter/pomodoro/lib/main.dart`
- Test: `fixtures/flutter/pomodoro/test/views/pomodoro_view_test.dart`

**Interfaces:**
- Consumes: `PomodoroViewModel` (Task 2).
- Produces: `PomodoroApp({TimerConfig config, Ticker? ticker})` — the injectable root Tasks 5–6 pump; keys `timer.display|phase|sessions|start|reset|skip`.

- [x] **Step 1: Failing widget tests** (`test/views/pomodoro_view_test.dart`) — same mocktail ticker harness as Task 2:

```dart
// Pump PomodoroApp(config: TimerConfig.demo(), ticker: mockTicker); then:
testWidgets('renders initial timer, phase label, and zero sessions', (t) async {
  // expect find.byKey(Key('timer.display')) shows '00:03' (demo work)
  // expect find.byKey(Key('timer.phase')) shows 'Focus'
  // expect find.byKey(Key('timer.sessions')) shows '0'
});
testWidgets('tapping start begins countdown; display updates per tick', (t) async {
  // tap timer.start; ticks.add(null); await t.pump();
  // expect display '00:02'
});
testWidgets('completing a work phase updates phase label and session count', (t) async {
  // tap start; 3 ticks; pump;
  // expect phase 'Short break', sessions '1'
});
testWidgets('reset returns the UI to the initial state', (t) async { /* tap reset after ticks */ });
```

Write these as real assertions (the comments above give the exact expectations); run → FAIL.

- [x] **Step 2: Implement the view** — single screen, Material 3, no packages:

```dart
import 'package:flutter/material.dart';

import '../domain/ticker.dart';
import '../domain/timer_config.dart';
import '../viewmodels/pomodoro_viewmodel.dart';

class PomodoroApp extends StatelessWidget {
  const PomodoroApp({super.key, this.config = const TimerConfig(), this.ticker});

  final TimerConfig config;
  final Ticker? ticker;

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Pomodoro',
        theme: ThemeData(colorSchemeSeed: Colors.red, useMaterial3: true),
        home: PomodoroView(config: config, ticker: ticker ?? SystemTicker()),
      );
}

class PomodoroView extends StatefulWidget {
  const PomodoroView({super.key, required this.config, required this.ticker});

  final TimerConfig config;
  final Ticker ticker;

  @override
  State<PomodoroView> createState() => _PomodoroViewState();
}

class _PomodoroViewState extends State<PomodoroView> with WidgetsBindingObserver {
  late final PomodoroViewModel vm =
      PomodoroViewModel(ticker: widget.ticker, config: widget.config);

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) => vm.onLifecycle(state);

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    vm.dispose();
    super.dispose();
  }

  static const _phaseLabels = {
    Phase.work: 'Focus',
    Phase.shortBreak: 'Short break',
    Phase.longBreak: 'Long break',
  };

  String _mmss(Duration d) =>
      '${(d.inMinutes).toString().padLeft(2, '0')}:${(d.inSeconds % 60).toString().padLeft(2, '0')}';

  @override
  Widget build(BuildContext context) => Scaffold(
        body: Center(
          child: ListenableBuilder(
            listenable: vm,
            builder: (context, _) => Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Text(_phaseLabels[vm.phase]!,
                    key: const Key('timer.phase'),
                    style: Theme.of(context).textTheme.titleLarge),
                Text(_mmss(vm.remaining),
                    key: const Key('timer.display'),
                    style: Theme.of(context).textTheme.displayLarge),
                Text('${vm.completedWork}',
                    key: const Key('timer.sessions'),
                    style: Theme.of(context).textTheme.titleMedium),
                const SizedBox(height: 24),
                Row(
                  mainAxisAlignment: MainAxisAlignment.center,
                  children: [
                    IconButton.filled(
                      key: const Key('timer.start'),
                      icon: Icon(vm.running ? Icons.pause : Icons.play_arrow),
                      onPressed: () => vm.running ? vm.pause() : vm.start(),
                    ),
                    IconButton(
                      key: const Key('timer.reset'),
                      icon: const Icon(Icons.restart_alt),
                      onPressed: vm.reset,
                    ),
                    IconButton(
                      key: const Key('timer.skip'),
                      icon: const Icon(Icons.skip_next),
                      onPressed: vm.skip,
                    ),
                  ],
                ),
              ],
            ),
          ),
        ),
      );
}
```

- [x] **Step 3: `lib/main.dart`** — demo mode is a real run mode:

```dart
import 'package:flutter/material.dart';

import 'domain/timer_config.dart';
import 'views/pomodoro_view.dart';

void main() {
  const demo = bool.fromEnvironment('DEMO_MODE');
  runApp(const PomodoroApp(config: demo ? TimerConfig.demo() : TimerConfig()));
}
```

- [x] **Step 4: Run — PASS**: `flutter test test/` and `flutter analyze` clean.
- [x] **Step 5: Commit** — `git commit -m "feat: pomodoro view with keyed controls and demo-mode entrypoint"`

### Task 4: Persona documentation + the persona↔journey guard

**Files:**
- Create: `fixtures/flutter/pomodoro/docs/personas/README.md`
- Create: `fixtures/flutter/pomodoro/docs/personas/maya-strict-cycles.md`
- Create: `fixtures/flutter/pomodoro/docs/personas/sam-interruptions.md`
- Create: `fixtures/flutter/pomodoro/docs/personas/rio-long-haul.md`
- Test: `fixtures/flutter/pomodoro/test/persona_mapping_test.dart`

**Interfaces:**
- Produces: the naming contract — persona file `<slug>.md` ⇄ journey file `integration_test/journeys/<slug_with_underscores>_journey_test.dart` — enforced mechanically by the guard test.

- [x] **Step 1: `docs/personas/README.md`** — the local convention:

```markdown
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
```

- [x] **Step 2: `docs/personas/maya-strict-cycles.md`:**

```markdown
# Maya — the strict-cycle deep worker

**Profile:** Backend developer; runs textbook pomodoros all morning. Trusts
the timer completely — never pauses, never skips, starts every phase herself
(auto-advance off), and expects the long break exactly when the cycle policy
says so.

**Goals:** uninterrupted focus blocks; an accurate session count at day's end.
**Frustrations:** timers that credit sessions wrongly or surprise her with the
wrong break type.

## Journey (config: `TimerConfig.demo()` — work 3s / short 2s / long 4s / long break every 2)

| # | Step | Expected state (asserted by key) |
|---|------|----------------------------------|
| 1 | Open the app | phase=Focus, display=00:03, sessions=0 |
| 2 | Start; let work phase complete | phase=Short break, sessions=1, stopped |
| 3 | Start the break; let it complete | phase=Focus, display=00:03 |
| 4 | Start; let 2nd work phase complete | phase=Long break (cycle policy), sessions=2 |
| 5 | Start the long break; let it complete | phase=Focus, sessions=2 |

**Invariants:** session count only increments on *completed work phases*;
break type is decided by `completedWork % cyclesPerLongBreak`; timer never
runs without Maya pressing start.
```

- [x] **Step 3: `docs/personas/sam-interruptions.md`:**

```markdown
# Sam — the interrupt-driven starter

**Profile:** Support engineer; gets pulled away constantly. Pauses mid-phase,
resumes, sometimes gives up and resets, and skips breaks he doesn't want.
The app must keep exact state through all of it.

**Goals:** the timer is exactly where he left it after every interruption.
**Frustrations:** pause/resume drift; resets that leave ghost state; skipped
phases that steal or grant session credit.

## Journey (config: `TimerConfig.demo()`)

| # | Step | Expected state |
|---|------|----------------|
| 1 | Start work; pause after ~1s | display frozen, stopped |
| 2 | Wait; confirm display unchanged | display identical (no drift while paused) |
| 3 | Resume; let work complete | phase=Short break, sessions=1 |
| 4 | Skip the break | phase=Focus, sessions=1 (skip grants nothing) |
| 5 | Start work, then reset mid-phase | phase=Focus, display=00:03, sessions=0, stopped |
| 6 | Background the app while running (lifecycle) | auto-paused, display frozen |

**Invariants:** pause is lossless; reset is total (state, count, phase);
skip never credits a session; backgrounding never lets time leak.
```

- [x] **Step 4: `docs/personas/rio-long-haul.md`:**

```markdown
# Rio — the custom-duration long-hauler

**Profile:** Writer; ignores the 25/5 default and configures long 50/10
blocks with a long break every 3. Runs many consecutive cycles in one
sitting. (Journey uses a compressed custom config with the same *shape*:
work 4s / short 2s / long 5s / every 3 — same policy, demo scale.)

**Goals:** the machine respects custom durations and the custom cycle policy
across a long run.
**Frustrations:** apps that hard-code 25/5 or mis-place the long break under
non-default policies.

## Journey (config: work 4s, short 2s, long 5s, cyclesPerLongBreak 3)

| # | Step | Expected state |
|---|------|----------------|
| 1 | Open with custom config | display=00:04 (custom work length respected) |
| 2 | Complete work #1 and its short break | sessions=1, back to Focus |
| 3 | Complete work #2 and its short break | sessions=2, back to Focus |
| 4 | Complete work #3 | phase=Long break (policy: every 3), sessions=3 |
| 5 | Complete the long break | phase=Focus, sessions=3 |

**Invariants:** durations come from config everywhere (display proves it);
long-break placement follows `cyclesPerLongBreak`, not a hard-coded 4.
```

- [x] **Step 5: The guard test** (`test/persona_mapping_test.dart`) — pure Dart IO, runs with the unit suite:

```dart
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
```

- [x] **Step 6: Run** — guard FAILS (journeys don't exist yet — correct: it's the red state Task 6 turns green). Commit docs + guard together: `git commit -m "docs: pomodoro personas as testable specs with persona-journey mapping guard"` (committing a red guard is acceptable only because Task 6 follows immediately in the same execution run; if execution pauses here, mark the test `skip:` with a TODO-Task-6 note instead).

### Task 5: Smoke test — the app boots

**Files:**
- Create: `fixtures/flutter/pomodoro/integration_test/smoke_test.dart`

- [x] **Step 1: Write it:**

```dart
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/main.dart' as app;

/// Smoke layer (tester-skill ladder): the real entrypoint boots, the first
/// screen renders, no crash. Nothing more — journeys own behavior.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('app boots and renders the timer screen', (tester) async {
    app.main();
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('timer.display')), findsOneWidget);
    expect(find.byKey(const Key('timer.start')), findsOneWidget);
    expect(find.byKey(const Key('timer.phase')), findsOneWidget);
  });
}
```

- [x] **Step 2: Run on the macOS desktop target** — `flutter test integration_test/smoke_test.dart -d macos` → PASS (first run compiles the macOS runner; allow several minutes). Optional richer smoke: drive the built app with probe-runner (`~/.agents/skills/probe-runner`) for a boot screenshot — worthwhile once, not per-run.
- [x] **Step 3: Commit** — `git commit -m "test: pomodoro boot smoke test on macos target"`

### Task 6: Persona E2E journeys

**Files:**
- Create: `fixtures/flutter/pomodoro/integration_test/journeys/helpers.dart`
- Create: `fixtures/flutter/pomodoro/integration_test/journeys/maya_strict_cycles_journey_test.dart`
- Create: `fixtures/flutter/pomodoro/integration_test/journeys/sam_interruptions_journey_test.dart`
- Create: `fixtures/flutter/pomodoro/integration_test/journeys/rio_long_haul_journey_test.dart`

**Interfaces:**
- Consumes: `PomodoroApp(config:, ticker:)` (Task 3), persona docs (Task 4 — each journey's steps mirror its doc's Journey table as comments).
- Produces: green `flutter test integration_test -d macos`; guard test from Task 4 goes green.

- [x] **Step 1: `helpers.dart`** — the one shared utility (poll-until-state, never sleep-and-hope):

```dart
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

/// Polls the live widget tree until [key]'s Text widget shows [text].
/// Journeys assert state transitions (phase labels, session counts) and use
/// this to WAIT — they never assert elapsed wall-clock time (flaky under load).
Future<void> waitForText(
  WidgetTester tester,
  String key,
  String text, {
  Duration timeout = const Duration(seconds: 15),
}) async {
  final deadline = DateTime.now().add(timeout);
  while (DateTime.now().isBefore(deadline)) {
    await tester.pump(const Duration(milliseconds: 100));
    final finder = find.byKey(Key(key));
    if (finder.evaluate().isNotEmpty &&
        (tester.widget<Text>(finder)).data == text) {
      return;
    }
  }
  fail('timed out waiting for $key == "$text"');
}

Future<void> tapKey(WidgetTester tester, String key) async {
  await tester.tap(find.byKey(Key(key)));
  await tester.pump();
}
```

- [x] **Step 2: Maya** (`maya_strict_cycles_journey_test.dart`) — spec: `docs/personas/maya-strict-cycles.md`:

```dart
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:pomodoro/domain/timer_config.dart';
import 'package:pomodoro/views/pomodoro_view.dart';

import 'helpers.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('Maya: strict cycles, correct break policy, honest session count',
      (tester) async {
    await tester.pumpWidget(const PomodoroApp(config: TimerConfig.demo()));
    await tester.pump();

    // 1. Open the app
    expect(find.text('Focus'), findsOneWidget);
    expect(find.text('00:03'), findsOneWidget);

    // 2. Start; let work phase complete
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Short break');
    await waitForText(tester, 'timer.sessions', '1');

    // 3. Start the break; let it complete
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');

    // 4. Start; 2nd work completion hits the long-break policy (every 2)
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Long break');
    await waitForText(tester, 'timer.sessions', '2');

    // 5. Long break completes; back to Focus, count untouched
    await tapKey(tester, 'timer.start');
    await waitForText(tester, 'timer.phase', 'Focus');
    await waitForText(tester, 'timer.sessions', '2');
  });
}
```

- [x] **Step 3: Sam** (`sam_interruptions_journey_test.dart`) — spec: `docs/personas/sam-interruptions.md`. Same harness; steps: start → `waitForText` display `00:02` → tap `timer.start` (pause) → `await tester.pump(const Duration(seconds: 2))` → assert display STILL `00:02` (frozen-while-paused: this is a state assertion, the pump is only a wait) → resume → wait for `Short break`/sessions `1` → tap `timer.skip` → assert `Focus` + sessions still `1` → start then tap `timer.reset` → assert `Focus`/`00:03`/sessions `0` → simulate lifecycle via `tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.paused)` then assert frozen display and, after `resumed`, still stopped. Every step carries its doc-table comment.

- [x] **Step 4: Rio** (`rio_long_haul_journey_test.dart`) — spec: `docs/personas/rio-long-haul.md`. Pump `PomodoroApp(config: TimerConfig(work: Duration(seconds: 4), shortBreak: Duration(seconds: 2), longBreak: Duration(seconds: 5), cyclesPerLongBreak: 3))`; assert initial display `00:04`; loop work+break twice asserting sessions 1 then 2 with `Short break` both times; complete work #3 → assert `Long break` + sessions `3`; complete it → `Focus`, sessions `3`.

- [x] **Step 5: Run everything** — `flutter test integration_test -d macos` → smoke + 3 journeys PASS (~1 min after first build; journeys are seconds each in demo time). `flutter test test/` → unit + widget + persona-mapping guard all PASS (guard now green).
- [x] **Step 6: Commit** — `git commit -m "test: persona e2e journeys for maya, sam, and rio in compressed demo time"`

### Task 7: Repo wiring — Make verbs + the reusable convention doc

**Files:**
- Modify: `Makefile` (nostos root)
- Create: `docs/testing/persona-e2e-baseline.md`
- Modify: `docs/plans/complete-nostos-fully-wired-operational.md` (Part VIII registry gains this plan's row)

- [x] **Step 1: Make targets** (append; keep `make ci` Rust-only):

```make
## fixture-test: flutter fixture unit/widget suites + persona-mapping guard
fixture-test:
	cd fixtures/flutter/pomodoro && flutter test test/

## fixture-e2e: smoke + persona journeys on the macOS desktop target
fixture-e2e:
	cd fixtures/flutter/pomodoro && flutter test integration_test -d macos
```

CI note: GitHub's macOS runners can run `fixture-e2e`, but the Rust pipeline must not pay that cost per-push — if CI coverage is wanted later, add a separate workflow triggered on `fixtures/**` paths only. Deliberately not added now (`ponytail:` local verbs suffice until a second fixture exists).

- [x] **Step 2: `docs/testing/persona-e2e-baseline.md`** — the recipe other fixtures follow:

```markdown
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
   patrol_cli is installed and the escalation is additive.
6. **Keys, not text.** Every asserted widget has a stable `Key('area.thing')`;
   copy changes must not break journeys.

When the nostos Flutter SDK lands (docs/plans/flutter-sdk.md), fixtures gain a
sync layer; persona journeys then double as SDK E2E: same personas, plus sync
assertions (offline write → reconnect → row echoed).
```

- [x] **Step 3: Verify the master-plan registry row** — `docs/plans/complete-nostos-fully-wired-operational.md` Part VIII already lists this plan as its first row (added at plan authoring); confirm it's intact and mark it done there once this plan completes.

- [x] **Step 4: Verify** — `make fixture-test` and `make fixture-e2e` green; `make ci` untouched and green.
- [x] **Step 5: Commit** — `git commit -m "feat: fixture make verbs, persona-e2e baseline convention doc, master plan registry entry"`

---

## Part II — Todo fixture: Supabase cloud + auth smoke (Tasks 8–10)

Same discipline as the Rust side of this repo: **fake and real adapters behind one port, env-gated live runs** (`FakeReplicator` vs `PgReplicator`, `NOSTOS_PG_URL`-gated e2e — this is the Flutter mirror of that pattern). All Supabase values are mocked until the operator provides credentials; the live path is real code, compiled always, selected only by env. Supabase is not incidental: nostos is Supabase-first (ADR-0007) and `SupabaseJwtAuth` already verifies Supabase JWTs on `/sync` — this fixture's auth session is the exact token the future SDK hands to nostos.

**Operator handoff (live mode activation — nothing below blocks on it):**
- [ ] Create a Supabase project; apply `fixtures/flutter/todo/supabase/schema.sql`.
- [ ] Create a pre-confirmed test user (email confirmation off, or confirm manually).
- [ ] Copy `env.example.json` → `env.json` (gitignored) and fill `SUPABASE_URL`, `SUPABASE_ANON_KEY`, `SUPABASE_TEST_EMAIL`, `SUPABASE_TEST_PASSWORD`.
- [ ] Run `make fixture-todo-smoke-live`.

> **Status (2026-07, v0.1 prep):** mock mode verified — `make fixture-todo-test`
> → 11/11 unit/widget tests pass; `make fixture-todo-smoke` → 2/2 integration
> tests pass (boot → reject bad password → authenticate → add todo → sign out).
> Live mode is real code, compiled always, selected only by `env.json`; the
> four checkboxes above remain operator-gated on Supabase credentials.
> `make fixture-todo-smoke-live` now fails fast with the exact setup steps if
> `env.json` is absent (commit `caf3619`).

### Task 8: Todo fixture scaffold — ports, fakes, Supabase adapters, env seam

**Files:**
- Create: `fixtures/flutter/todo/` (via `flutter create --project-name todo --org run.nostos --platforms macos,ios,android --empty fixtures/flutter/todo`)
- Create: `fixtures/flutter/todo/lib/env.dart`
- Create: `fixtures/flutter/todo/lib/domain/auth_gateway.dart`, `lib/domain/todo_repository.dart`
- Create: `fixtures/flutter/todo/lib/infra/fake_auth_gateway.dart`, `lib/infra/in_memory_todo_repository.dart`
- Create: `fixtures/flutter/todo/lib/infra/supabase_auth_gateway.dart`, `lib/infra/supabase_todo_repository.dart`
- Create: `fixtures/flutter/todo/supabase/schema.sql`, `fixtures/flutter/todo/env.example.json`
- Modify: `fixtures/flutter/todo/pubspec.yaml`, `fixtures/flutter/todo/.gitignore` (add `env.json`)

**Interfaces:**
- Produces (later tasks depend on these exact names): `Env.{supabaseUrl,supabaseAnonKey,testEmail,testPassword,isLive}`; `Session {userId, email}`; `AuthGateway {restore(), signIn(email,password), signOut()}` throwing `AuthException.invalidCredentials | .network(msg)`; `Todo {id,title,done}`; `TodoRepository {watch(), add(title), toggle(id)}`; `FakeAuthGateway.demoEmail/demoPassword` constants.

- [x] **Step 1: Scaffold + deps.** Before adding, verify the current major on pub.dev (read-the-damn-docs; app-example pins `supabase_flutter ^2.15.1` as of 2026-06 — expect 2.x):

```bash
cd fixtures/flutter/todo && flutter pub add supabase_flutter && flutter pub add dev:mocktail dev:integration_test --sdk=flutter
```

(`integration_test` is an SDK package: declare it in `dev_dependencies` with `sdk: flutter`, as in the pomodoro fixture's pubspec.)

- [x] **Step 2: `lib/env.dart`** — the single mock/live switch, compile-time:

```dart
/// Live-mode switch. Values arrive ONLY via --dart-define / --dart-define-from-file
/// (see env.example.json); nothing is ever hardcoded or committed.
class Env {
  static const supabaseUrl = String.fromEnvironment('SUPABASE_URL');
  static const supabaseAnonKey = String.fromEnvironment('SUPABASE_ANON_KEY');
  static const testEmail = String.fromEnvironment('SUPABASE_TEST_EMAIL');
  static const testPassword = String.fromEnvironment('SUPABASE_TEST_PASSWORD');

  /// Both-or-neither: the smoke suite fails closed on a contradictory env
  /// (exactly one of url/key set) — see smoke_auth_test.dart's guard.
  static const isLive = supabaseUrl != '' && supabaseAnonKey != '';
}
```

- [x] **Step 3: Domain ports** (`lib/domain/auth_gateway.dart`):

```dart
class Session {
  const Session({required this.userId, required this.email});
  final String userId;
  final String email;
}

sealed class AuthException implements Exception {
  const AuthException();
  const factory AuthException.invalidCredentials() = InvalidCredentials;
  const factory AuthException.network(String message) = NetworkAuthException;
}

final class InvalidCredentials extends AuthException {
  const InvalidCredentials();
}

final class NetworkAuthException extends AuthException {
  const NetworkAuthException(this.message);
  final String message;
}

/// Auth port. Fake by default; Supabase adapter when Env.isLive.
abstract interface class AuthGateway {
  Future<Session?> restore();
  Future<Session> signIn(String email, String password);
  Future<void> signOut();
}
```

and (`lib/domain/todo_repository.dart`):

```dart
class Todo {
  const Todo({required this.id, required this.title, this.done = false});
  final String id;
  final String title;
  final bool done;
}

abstract interface class TodoRepository {
  Stream<List<Todo>> watch();
  Future<void> add(String title);
  Future<void> toggle(String id);
}
```

- [x] **Step 4: Fakes** — `FakeAuthGateway` accepts exactly `demoEmail`/`demoPassword` (`demo@nostos.run` / `demo-1234`, exposed as static consts so the smoke test and any human demoing the app use the same values), throws `AuthException.invalidCredentials()` otherwise, `restore()` returns null; `InMemoryTodoRepository` holds a list behind a broadcast `StreamController`, re-emitting on every mutation. Both are ~30 lines; write them fully.

- [x] **Step 5: Supabase adapters** — real code, only *constructed* in live mode (verify exact API against the installed supabase_flutter 2.x docs before writing — the shapes below are the v2 API as of 2026-06):

```dart
// supabase_auth_gateway.dart
class SupabaseAuthGateway implements AuthGateway {
  SupabaseAuthGateway(this._client);
  final SupabaseClient _client;

  @override
  Future<Session?> restore() async {
    final s = _client.auth.currentSession;
    final u = s?.user;
    return u == null ? null : Session(userId: u.id, email: u.email ?? '');
  }

  @override
  Future<Session> signIn(String email, String password) =>
      _withRetry(() async {
        try {
          final res = await _client.auth
              .signInWithPassword(email: email, password: password);
          final u = res.user!;
          return Session(userId: u.id, email: u.email ?? '');
        } on AuthApiException catch (e) {
          // Wrong creds are NOT retried — only transport failures are.
          throw e.statusCode == '400'
              ? const AuthException.invalidCredentials()
              : AuthException.network(e.message);
        }
      });

  @override
  Future<void> signOut() => _client.auth.signOut();

  /// ponytail: 2 retries, 1s/2s backoff, network errors only — enough to
  /// absorb transient Supabase 5xx in the live smoke; a real app grows a
  /// proper retry policy with the SDK retrofit.
  Future<T> _withRetry<T>(Future<T> Function() op) async {
    for (var attempt = 0; ; attempt++) {
      try {
        return await op();
      } on NetworkAuthException {
        if (attempt >= 2) rethrow;
        await Future<void>.delayed(Duration(seconds: 1 << attempt));
      }
    }
  }
}
```

`SupabaseTodoRepository`: `watch()` via `_client.from('todos').stream(primaryKey: ['id']).order('created_at')` mapped to `Todo`; `add` inserts `{title, user_id: _client.auth.currentUser!.id}`; `toggle` flips `done` by id. Write fully at implementation time against the installed version's docs.

- [x] **Step 6: `supabase/schema.sql`** — the contract the operator applies:

```sql
create table if not exists todos (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references auth.users (id) on delete cascade,
  title text not null,
  done boolean not null default false,
  created_at timestamptz not null default now()
);

alter table todos enable row level security;

create policy "owner full access" on todos
  for all using (auth.uid() = user_id) with check (auth.uid() = user_id);
```

RLS owner-only also isolates the live smoke's writes to the dedicated test user — no cross-talk with any other data in the project.

- [x] **Step 7: `env.example.json`** (committed) + gitignore `env.json`:

```json
{
  "SUPABASE_URL": "https://YOUR-PROJECT.supabase.co",
  "SUPABASE_ANON_KEY": "YOUR-ANON-KEY",
  "SUPABASE_TEST_EMAIL": "PRE-PROVISIONED-TEST-USER@example.com",
  "SUPABASE_TEST_PASSWORD": "ITS-PASSWORD"
}
```

Note in the fixture README: the test user must exist and be email-confirmed BEFORE the live smoke runs (the suite fails with a provisioning message otherwise, it does not create users).

- [x] **Step 8: Verify** — `flutter analyze` clean (adapters compile in mock mode too). Commit: `git commit -m "feat: todo fixture scaffold with auth and repository ports, fakes, supabase adapters, env seam"`

### Task 9: Sign-in + todo views, viewmodels, unit/widget tests

**Files:**
- Create: `fixtures/flutter/todo/lib/viewmodels/auth_viewmodel.dart`, `lib/viewmodels/todo_viewmodel.dart`
- Create: `fixtures/flutter/todo/lib/views/sign_in_view.dart`, `lib/views/todo_view.dart`
- Modify: `fixtures/flutter/todo/lib/main.dart`
- Test: `fixtures/flutter/todo/test/viewmodels/auth_viewmodel_test.dart`, `test/viewmodels/todo_viewmodel_test.dart`, `test/views/sign_in_view_test.dart`

**Interfaces:**
- Consumes: ports + fakes (Task 8).
- Produces: `TodoApp({required AuthGateway auth, required TodoRepository todos})` root widget; keys `auth.email`, `auth.password`, `auth.submit`, `auth.error`, `auth.signout`, `todos.input`, `todos.add`, `todos.list`.

- [x] **Step 1: Failing unit tests first** (mocktail on the ports, pomodoro Task 2 harness style): `AuthViewModel` — signIn success exposes `session` and clears busy; `invalidCredentials` exposes `errorMessage` ('Invalid email or password'), never throws to the view; network error exposes the network message; signOut clears session. `TodoViewModel` — `watch` stream re-renders list; `add` delegates with trimmed title, ignores empty; `toggle` delegates by id. Run → FAIL.
- [x] **Step 2: Implement the two ViewModels** (plain `ChangeNotifier`, exactly the states the tests pin — no more).
- [x] **Step 3: Failing widget test** — `SignInView` with a mocktail `AuthGateway` throwing `invalidCredentials`: enter text in `auth.email`/`auth.password`, tap `auth.submit`, expect `auth.error` visible with the message. Run → FAIL.
- [x] **Step 4: Views + composition.** `SignInView`: two `TextField`s + `FilledButton` + error `Text` (keys above). `TodoView`: `TextField(key: todos.input)` + add `IconButton(key: todos.add)` + `ListView(key: todos.list)` of `CheckboxListTile`s + sign-out `IconButton(key: auth.signout)`. `TodoApp` swaps SignInView↔TodoView on the auth VM's session. `main.dart`:

```dart
Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  late final AuthGateway auth;
  late final TodoRepository todos;
  if (Env.isLive) {
    await Supabase.initialize(url: Env.supabaseUrl, anonKey: Env.supabaseAnonKey);
    final client = Supabase.instance.client;
    auth = SupabaseAuthGateway(client);
    todos = SupabaseTodoRepository(client);
  } else {
    auth = FakeAuthGateway();
    todos = InMemoryTodoRepository();
  }
  runApp(TodoApp(auth: auth, todos: todos));
}
```

- [x] **Step 5: All `flutter test test/` green; `flutter analyze` clean. Commit** — `git commit -m "feat: todo fixture sign-in and list views with viewmodels over mocked ports"`

### Task 10: The dual-mode smoke test — app + cloud + authentication

**Files:**
- Create: `fixtures/flutter/todo/integration_test/smoke_auth_test.dart`
- Modify: `Makefile` (nostos root — todo fixture verbs)

**Interfaces:**
- Consumes: `TodoApp` wiring via the real `main()` (mock mode) and `Env` (Task 8), keys (Task 9).
- Produces: ONE test file, two modes. Mock mode: CI-safe, runs today with zero credentials. Live mode: same steps against real Supabase cloud auth + database, activated purely by `--dart-define-from-file=env.json`.

- [x] **Step 1: Write it** (single file — one step list, no mock/live drift; advisor-mandated guard first):

```dart
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:todo/env.dart';
import 'package:todo/infra/fake_auth_gateway.dart';
import 'package:todo/main.dart' as app;

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  // Fail-closed branch guard: a contradictory env (exactly one of url/key)
  // must abort loudly, never silently fall back to mock and "pass".
  test('adapter selection matches the provided environment', () {
    final url = Env.supabaseUrl.isNotEmpty;
    final key = Env.supabaseAnonKey.isNotEmpty;
    expect(url == key, isTrue,
        reason: 'contradictory env: provide BOTH SUPABASE_URL and '
            'SUPABASE_ANON_KEY (live) or NEITHER (mock)');
    expect(Env.isLive, url && key);
    if (Env.isLive) {
      expect(Env.testEmail.isNotEmpty && Env.testPassword.isNotEmpty, isTrue,
          reason: 'live mode needs SUPABASE_TEST_EMAIL/PASSWORD for a '
              'PRE-PROVISIONED, email-confirmed test user (the suite '
              'does not create users)');
    }
  });

  final email = Env.isLive ? Env.testEmail : FakeAuthGateway.demoEmail;
  final password = Env.isLive ? Env.testPassword : FakeAuthGateway.demoPassword;

  testWidgets(
    'smoke: boot -> reject bad password -> authenticate -> add todo -> sign out',
    timeout: const Timeout(Duration(seconds: 30)), // live-mode latency budget
    (tester) async {
      app.main();
      await tester.pumpAndSettle();

      // Boot: sign-in screen renders.
      expect(find.byKey(const Key('auth.email')), findsOneWidget);

      // Wrong password is rejected with a visible error (real round-trip in
      // live mode; FakeAuthGateway path in mock mode — same assertion).
      await tester.enterText(find.byKey(const Key('auth.email')), email);
      await tester.enterText(
          find.byKey(const Key('auth.password')), 'wrong-password');
      await tester.tap(find.byKey(const Key('auth.submit')));
      await _settle(tester);
      expect(find.byKey(const Key('auth.error')), findsOneWidget);

      // Correct credentials authenticate into the todo home.
      await tester.enterText(
          find.byKey(const Key('auth.password')), password);
      await tester.tap(find.byKey(const Key('auth.submit')));
      await _settle(tester);
      expect(find.byKey(const Key('todos.list')), findsOneWidget);

      // A write lands and renders (in live mode this is a real insert under
      // RLS as the test user, streamed back from Supabase).
      final marker = 'smoke ${DateTime.now().millisecondsSinceEpoch}';
      await tester.enterText(find.byKey(const Key('todos.input')), marker);
      await tester.tap(find.byKey(const Key('todos.add')));
      await _settle(tester);
      expect(find.text(marker), findsOneWidget);

      // Sign out returns to the sign-in screen.
      await tester.tap(find.byKey(const Key('auth.signout')));
      await _settle(tester);
      expect(find.byKey(const Key('auth.email')), findsOneWidget);
    },
  );
}

/// pumpAndSettle with a poll loop tolerant of live-mode stream latency.
Future<void> _settle(WidgetsTester tester) async {
  for (var i = 0; i < 50; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}
```

(Adjust `_settle` to the pomodoro fixture's `waitForText` helper style if plain fixed polling proves flaky in live mode — poll-for-state beats fixed loops; the executor picks whichever the live run proves out, and the unique `marker` string keeps repeat live runs from colliding with prior rows.)

- [x] **Step 2: Make verbs** (nostos root Makefile, next to the pomodoro verbs):

```make
## fixture-todo-test: todo fixture unit/widget suites (mocked ports)
fixture-todo-test:
	cd fixtures/flutter/todo && flutter test test/

## fixture-todo-smoke: dual-mode smoke, MOCK mode (no credentials needed)
fixture-todo-smoke:
	cd fixtures/flutter/todo && flutter test integration_test/smoke_auth_test.dart -d macos

## fixture-todo-smoke-live: same smoke against real Supabase (needs env.json — see env.example.json)
fixture-todo-smoke-live:
	cd fixtures/flutter/todo && flutter test integration_test/smoke_auth_test.dart -d macos --dart-define-from-file=env.json
```

- [x] **Step 3: Verify mock mode end-to-end today** — `make fixture-todo-test` and `make fixture-todo-smoke` green with zero credentials. Live mode stays parked at the Operator handoff checklist above; the suite's guard messages tell the operator exactly what's missing if run early.
- [x] **Step 4: Commit** — `git commit -m "test: dual-mode supabase auth smoke for todo fixture — mocked now, live on operator credentials"`

---

## Risks

1. **[HIGH] Wall-clock flakiness** — the whole design routes around it (poll-for-state, transition assertions); the one legitimate wait-then-assert-frozen step (Sam #2) asserts *absence of change*, which is load-tolerant. Any future journey asserting elapsed time is a review-blocker.
2. **[MED] First macOS integration build is slow** (full runner compile) and needs Xcode signing defaults — verified working on this machine via app-example precedent; CI would need a macOS runner (deliberately deferred).
3. **[MED] Compressed-time blind spots** — 3-second phases can't surface bugs that only exist at scale (int overflow won't, but notification scheduling might, later). Mitigated by the equivalence test + keeping `demo()` shape-identical to real configs; revisit when native surfaces (notifications) arrive with Patrol.
4. **[LOW] Persona/doc drift** — mechanically guarded (Task 4 test); the doc-table-as-comments convention keeps the human-readable spec reviewable against the executable one.
5. **[LOW] Fixture scope creep** — the fixtures stay SDK-free until `flutter-sdk.md` opens; anything nostos-sync-shaped lands there, not here.
6. **[CRITICAL→mitigated] Silent mock fallback in the todo smoke** — a typo'd env would otherwise make the "live" run pass in mock mode; the fail-closed branch guard (Task 10 Step 1) is the first test in the file for exactly this reason.
7. **[HIGH] Live-mode flakiness** — Supabase latency/transient 5xx: 30s per-test timeout + network-only retry with backoff in the adapter (never retrying invalid-credential responses). Repeat live runs can't collide: each run's todo carries a unique timestamp marker and RLS confines everything to the dedicated test user.
8. **[MED] Secrets hygiene** — `env.json` is gitignored, values flow only through `--dart-define-from-file`, nothing is hardcoded; `env.example.json` carries placeholders only. The live smoke never creates or deletes users.

## Execution notes

- Part I: strictly sequential Tasks 1→6, Task 7 last. Part II (Tasks 8→10) is independent of Part I and may run before, after, or in parallel with it; within Part II the order is 8→9→10. One executor session suffices.
- Part II is fully verifiable TODAY in mock mode (zero credentials); live mode activates via the Operator handoff checklist and changes no code — only `env.json`.
- Everything runs inside `/Volumes/developer_ssd/Developer/nostos`; the fixture is committed to the nostos repo including platform scaffolding (Flutter's generated `.gitignore` inside the fixture already excludes `build/`, `.dart_tool/`).
- Task 4's guard is committed red only if Task 6 follows in the same run — otherwise mark it `skip:` (see Task 4 Step 6).
- Nothing here publishes, deploys, or touches anything outside the nostos tree.
