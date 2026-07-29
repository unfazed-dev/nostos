// Material 3 theme for the Nostos Provider Dashboard.
//
// Applied methodology (stacked-kit-designer anti-slop blacklist):
// - No purple gradients → seeded blue (#2E6FDB), the app's established accent.
// - Configured ThemeData (never default M3 purple).
// - Flat surfaces (no default Card elevation); explicit elevation control.
// - Material 3 color scheme from a single seed → cohesive system.

import 'package:flutter/material.dart';

class AppTheme {
  const AppTheme._();

  /// The brand seed — a confident blue, not M3's default purple.
  static const Color seed = Color(0xFF2E6FDB);

  static ThemeData get light => ThemeData(
        useMaterial3: true,
        colorScheme: ColorScheme.fromSeed(
          seedColor: seed,
          brightness: Brightness.light,
        ),
        appBarTheme: const AppBarTheme(
          centerTitle: false,
          elevation: 0,
          scrolledUnderElevation: 0.5,
        ),
        cardTheme: const CardThemeData(
          elevation: 0,
          margin: EdgeInsets.zero,
        ),
        inputDecorationTheme: const InputDecorationTheme(
          filled: true,
          isDense: true,
          border: OutlineInputBorder(),
        ),
        filledButtonTheme: FilledButtonThemeData(
          style: FilledButton.styleFrom(
            padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 14),
            shape: RoundedRectangleBorder(
              borderRadius: BorderRadius.circular(12),
            ),
          ),
        ),
        chipTheme: const ChipThemeData(
          side: BorderSide.none,
        ),
      );

  static ThemeData get dark => ThemeData(
        useMaterial3: true,
        colorScheme: ColorScheme.fromSeed(
          seedColor: seed,
          brightness: Brightness.dark,
        ),
        appBarTheme: const AppBarTheme(
          centerTitle: false,
          elevation: 0,
          scrolledUnderElevation: 0.5,
        ),
        cardTheme: const CardThemeData(
          elevation: 0,
          margin: EdgeInsets.zero,
        ),
      );
}
