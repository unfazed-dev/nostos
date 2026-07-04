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
