// Shared UI widgets for the Nostos Provider Dashboard.
//
// Anti-slop: no stock Icons slop without theme tinting, no decorative gradients,
// no unconfigured defaults. Each widget earns its place.

import 'package:flutter/material.dart';

import '../models.dart';

/// Connection-state indicator chip for the app bar.
class ConnectionBadge extends StatelessWidget {
  const ConnectionBadge({super.key, required this.state, this.paused = false});
  final String state;
  final bool paused;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final (color, icon, label) = switch (state) {
      'connected' => (Colors.green, Icons.cloud_done, state),
      'connecting' => (Colors.orange, Icons.sync, state),
      'reconnecting' => (Colors.orange, Icons.sync_problem, state),
      _ => (scheme.outline, Icons.cloud_off, 'offline'),
    };
    return Padding(
      padding: const EdgeInsets.only(right: 4),
      child: Chip(
        avatar: Icon(icon, color: color, size: 16),
        label: Text(label,
            style: TextStyle(
                fontSize: 11,
                fontWeight: FontWeight.w600,
                color: scheme.onSurfaceVariant)),
        side: BorderSide.none,
        padding: const EdgeInsets.symmetric(horizontal: 4),
      ),
    );
  }
}

/// A pill showing a provider's rate, color-coded by rate type.
class RateBadge extends StatelessWidget {
  const RateBadge({super.key, required this.provider});
  final Provider provider;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = switch (provider.rateType) {
      RateType.hourly => scheme.primaryContainer,
      RateType.flat => Color(0xFFFFE0B2), // warm amber for flat
      RateType.subscription => Color(0xFFE1BEE7), // soft violet for sub
    };
    final onColor = switch (provider.rateType) {
      RateType.hourly => scheme.onPrimaryContainer,
      RateType.flat => Colors.brown.shade900,
      RateType.subscription => Colors.purple.shade900,
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
        color: color,
        borderRadius: BorderRadius.circular(20),
      ),
      child: Text(
        '${provider.rateType.label} · ${provider.rateLabel}',
        style: TextStyle(
            fontSize: 12, fontWeight: FontWeight.w600, color: onColor),
      ),
    );
  }
}

/// Status chip with a color matched to the status string.
class StatusChip extends StatelessWidget {
  const StatusChip({super.key, required this.status, this.positive = false});
  final String status;
  final bool positive;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = positive
        ? Colors.green.shade100
        : (status == 'issued' || status == 'confirmed'
            ? scheme.primaryContainer
            : Colors.grey.shade200);
    final onColor = positive
        ? Colors.green.shade900
        : (status == 'issued' || status == 'confirmed'
            ? scheme.onPrimaryContainer
            : Colors.grey.shade800);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 3),
      decoration: BoxDecoration(
        color: color,
        borderRadius: BorderRadius.circular(12),
      ),
      child: Text(status,
          style: TextStyle(
              fontSize: 11,
              fontWeight: FontWeight.w700,
              color: onColor,
              letterSpacing: 0.3)),
    );
  }
}

/// A circular avatar with the entity's initials, tinted by the given color.
class InitialsAvatar extends StatelessWidget {
  const InitialsAvatar({
    super.key,
    required this.initials,
    this.color = const Color(0xFF2E6FDB),
    this.size = 40,
  });
  final String initials;
  final Color color;
  final double size;

  @override
  Widget build(BuildContext context) => Container(
        width: size,
        height: size,
        decoration: BoxDecoration(
          color: color.withValues(alpha: 0.15),
          shape: BoxShape.circle,
        ),
        alignment: Alignment.center,
        child: Text(
          initials,
          style: TextStyle(
            color: color,
            fontWeight: FontWeight.w700,
            fontSize: size * 0.36,
          ),
        ),
      );
}

/// Empty-state placeholder: icon + message, centered.
class EmptyState extends StatelessWidget {
  const EmptyState({
    super.key,
    required this.icon,
    required this.message,
  });
  final IconData icon;
  final String message;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(32),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 48, color: scheme.outline.withValues(alpha: 0.5)),
            const SizedBox(height: 12),
            Text(
              message,
              style: TextStyle(
                  color: scheme.outline, fontSize: 14),
              textAlign: TextAlign.center,
            ),
          ],
        ),
      ),
    );
  }
}

/// A short FK id chip: "abc12345…" (the full UUID is uninformative in a list).
String shortId(String id) =>
    id.length > 8 ? '${id.substring(0, 8)}…' : id;
