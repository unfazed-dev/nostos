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
