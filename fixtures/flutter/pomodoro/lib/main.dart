import 'package:flutter/material.dart';

import 'domain/timer_config.dart';
import 'views/pomodoro_view.dart';

void main() {
  const demo = bool.fromEnvironment('DEMO_MODE');
  runApp(const PomodoroApp(config: demo ? TimerConfig.demo() : TimerConfig()));
}
