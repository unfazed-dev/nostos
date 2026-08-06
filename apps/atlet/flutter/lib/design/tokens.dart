import 'package:flutter/material.dart';

/// Design tokens transcribed verbatim from apps/atlet/design/design-system.md
/// (source: v1 styles.css :root, preserved verbatim). `accent` is the one
/// TweaksPanel-mutable value in the source doc; kept as a static const here
/// since Atlet has no runtime theming surface.
abstract final class AtletTokens {
  static const bone = Color(0xFFF5F0E8);
  static const bone2 = Color(0xFFEAE3D6);
  static const paper = Color(0xFFFBF8F2);
  static const ink = Color(0xFF1A1714);
  static const ink3 = Color(0xFF6E6760);
  static const rule = Color(0xFFD8CFBE);
  static const accent = Color(0xFFD2522B);
  static const accent2 = Color(0xFFB8431F);
  static const good = Color(0xFF4A7C3A);
  static const warn = Color(0xFFC68D2E);
  // Sans: Lexend 300-700; Mono numerals: JetBrains Mono. HIG: 34/22/17/13.
  // ponytail: font families named, not bundled (brief's dep list has no
  // google_fonts/asset fonts) — falls back to platform default until the
  // fonts are added as assets.
  static const sansFamily = 'Lexend';
  static const monoFamily = 'JetBrains Mono';

  static const double largeTitle = 34;
  static const double title2 = 22;
  static const double body = 17;
  static const double footnote = 13;
}
