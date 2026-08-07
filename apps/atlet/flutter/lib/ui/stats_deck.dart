import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';

/// Stats deck — ported from apps/atlet/design/home.jsx (StatsDeck: week
/// volume bars + streak + trend line). The design rendered static
/// WEEK_VOLUME/TREND_POINTS fixtures; here every figure is computed live
/// from the synced [SessionRow] list so the deck reflects nostos state.
class StatsDeck extends StatelessWidget {
  const StatsDeck({super.key, required this.sessions});

  final List<SessionRow> sessions;

  @override
  Widget build(BuildContext context) {
    final now = DateTime.now();
    final monday = DateTime(now.year, now.month, now.day)
        .subtract(Duration(days: now.weekday - 1));

    // Sessions per weekday, current week.
    final counts = List<int>.filled(7, 0);
    for (final s in sessions) {
      final d = DateTime(s.occurredOn.year, s.occurredOn.month, s.occurredOn.day);
      final offset = d.difference(monday).inDays;
      if (offset >= 0 && offset < 7) counts[offset]++;
    }
    final maxCount = counts.fold(0, (a, b) => a > b ? a : b);

    final streak = sessions.fold(0, (a, s) => s.streak > a ? s.streak : a);

    // Trend: last 10 sessions by date, metric normalized.
    final byDate = [...sessions]
      ..sort((a, b) => a.occurredOn.compareTo(b.occurredOn));
    final trend = byDate.length > 10
        ? byDate.sublist(byDate.length - 10)
        : byDate;

    return Container(
      key: const Key('stats-deck'),
      padding: const EdgeInsets.all(16),
      decoration: BoxDecoration(
        color: AtletTokens.paper,
        borderRadius: BorderRadius.circular(16),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.spaceBetween,
            children: [
              Text(
                'THIS WEEK',
                style: TextStyle(
                  fontSize: AtletTokens.footnote,
                  letterSpacing: 1.5,
                  fontWeight: FontWeight.w500,
                  color: AtletTokens.ink3,
                ),
              ),
              Row(
                children: [
                  Icon(Icons.local_fire_department_outlined,
                      size: 16, color: AtletTokens.warn),
                  const SizedBox(width: 4),
                  Text('$streak-day streak',
                      style: TextStyle(
                          color: AtletTokens.ink3,
                          fontSize: AtletTokens.footnote)),
                ],
              ),
            ],
          ),
          const SizedBox(height: 12),
          SizedBox(
            height: 56,
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                for (final (i, label) in const ['M', 'T', 'W', 'T', 'F', 'S', 'S'].indexed) ...[
                  if (i > 0) const SizedBox(width: 8),
                  Expanded(
                    child: Column(
                      mainAxisAlignment: MainAxisAlignment.end,
                      children: [
                        Container(
                          height: maxCount == 0
                              ? 3
                              : 3 + 32.0 * (counts[i] / maxCount),
                          decoration: BoxDecoration(
                            color: i == now.weekday - 1
                                ? AtletTokens.accent
                                : counts[i] > 0
                                    ? AtletTokens.accent.withValues(alpha: 0.45)
                                    : AtletTokens.ink.withValues(alpha: 0.08),
                            borderRadius: BorderRadius.circular(2),
                          ),
                        ),
                        const SizedBox(height: 4),
                        Text(label,
                            style: TextStyle(
                                fontSize: 10,
                                height: 1.0,
                                color: AtletTokens.ink3)),
                      ],
                    ),
                  ),
                ],
              ],
            ),
          ),
          if (trend.length >= 2) ...[
            const SizedBox(height: 12),
            SizedBox(
              height: 32,
              width: double.infinity,
              child: CustomPaint(
                painter: _TrendPainter(
                  values: [for (final s in trend) s.metric.toDouble()],
                  color: AtletTokens.accent,
                ),
              ),
            ),
          ],
        ],
      ),
    );
  }
}

class _TrendPainter extends CustomPainter {
  _TrendPainter({required this.values, required this.color});

  final List<double> values;
  final Color color;

  @override
  void paint(Canvas canvas, Size size) {
    if (values.length < 2) return;
    var lo = values.first, hi = values.first;
    for (final v in values) {
      if (v < lo) lo = v;
      if (v > hi) hi = v;
    }
    final span = (hi - lo) == 0 ? 1.0 : hi - lo;
    final path = Path();
    for (final (i, v) in values.indexed) {
      final x = size.width * i / (values.length - 1);
      final y = size.height - (size.height - 4) * ((v - lo) / span) - 2;
      i == 0 ? path.moveTo(x, y) : path.lineTo(x, y);
    }
    canvas.drawPath(
      path,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 2
        ..strokeCap = StrokeCap.round
        ..color = color,
    );
  }

  @override
  bool shouldRepaint(_TrendPainter old) =>
      old.values != values || old.color != color;
}
