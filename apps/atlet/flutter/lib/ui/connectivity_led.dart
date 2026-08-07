import 'package:flutter/material.dart';

import '../design/tokens.dart';

/// App-wide reactive connectivity state. Driven by the ConnectivityGuard
/// callback in main.dart: true = online, false = offline.
final ValueNotifier<bool> connectivityOnline = ValueNotifier<bool>(true);

/// Compact LED + label for AppBars: green "Online" / red "Offline".
/// Rebuilds reactively off [connectivityOnline] — no setState plumbing needed.
class ConnectivityLed extends StatelessWidget {
  const ConnectivityLed({super.key});

  @override
  Widget build(BuildContext context) {
    return ValueListenableBuilder<bool>(
      valueListenable: connectivityOnline,
      builder: (context, online, _) {
        final color = online
            ? const Color(0xFF2E9E5B) // green
            : const Color(0xFFD64545); // red
        return Padding(
          key: const Key('connectivity-led'),
          padding: const EdgeInsets.only(right: 12),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              AnimatedContainer(
                duration: const Duration(milliseconds: 250),
                width: 10,
                height: 10,
                decoration: BoxDecoration(
                  shape: BoxShape.circle,
                  color: color,
                  boxShadow: [
                    BoxShadow(
                      color: color.withValues(alpha: 0.5),
                      blurRadius: 6,
                      spreadRadius: 1,
                    ),
                  ],
                ),
              ),
              const SizedBox(width: 6),
              Text(
                online ? 'Online' : 'Offline',
                key: const Key('connectivity-led-label'),
                style: TextStyle(
                  fontSize: 12,
                  fontWeight: FontWeight.w600,
                  color: AtletTokens.ink,
                ),
              ),
            ],
          ),
        );
      },
    );
  }
}
