import 'dart:async';

import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';

/// Workout player — ported from apps/atlet/design/detail.jsx
/// (TimeDetail / RepsDetail / DistanceDetail + RestInterlude + FeedbackSheet).
///
/// [SessionRow] carries no steps column, so the player derives its plan from
/// the row itself: `time` counts down `metric` seconds, `reps` counts up to
/// `metric` reps, `distance` runs a stopwatch against `metric` km. Feedback
/// is display-only (no persistence surface on [SyncAdapter]) — the sheet
/// matches the design's emoji + tags flow and the snackbar names what was
/// actually stored: nothing but the log entry that already exists.
class WorkoutPlayer extends StatelessWidget {
  const WorkoutPlayer({super.key, required this.session});

  final SessionRow session;

  @override
  Widget build(BuildContext context) {
    return switch (session.type) {
      'time' => _TimePlayer(session: session),
      'reps' => _RepsPlayer(session: session),
      'distance' => _DistancePlayer(session: session),
      _ => _TimePlayer(session: session), // unknown types run as a timer
    };
  }
}

// ---------------------------------------------------------------------------
// Shared scaffold + dial
// ---------------------------------------------------------------------------

class _PlayerScaffold extends StatelessWidget {
  const _PlayerScaffold({required this.title, required this.child, this.controls});

  final String title;
  final Widget child;
  final Widget? controls;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: AtletTokens.bone,
      appBar: AppBar(
        backgroundColor: AtletTokens.bone,
        elevation: 0,
        title: Text(title, style: TextStyle(color: AtletTokens.ink)),
      ),
      body: SafeArea(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Column(
            children: [
              const Spacer(),
              child,
              const Spacer(),
              ?controls,
            ],
          ),
        ),
      ),
    );
  }
}

class _Dial extends StatelessWidget {
  const _Dial({required this.progress, this.urgent = false, required this.child});

  final double progress; // 0..1
  final bool urgent;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: 240,
      height: 240,
      child: Stack(
        alignment: Alignment.center,
        children: [
          SizedBox(
            width: 240,
            height: 240,
            child: CircularProgressIndicator(
              value: progress.clamp(0.0, 1.0),
              strokeWidth: 10,
              strokeCap: StrokeCap.round,
              backgroundColor: AtletTokens.ink.withValues(alpha: 0.08),
              color: urgent ? AtletTokens.accent2 : AtletTokens.accent,
            ),
          ),
          child,
        ],
      ),
    );
  }
}

String _fmtSec(int s) {
  final m = s ~/ 60;
  final r = s % 60;
  return '$m:${r.toString().padLeft(2, '0')}';
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

class _TimePlayer extends StatefulWidget {
  const _TimePlayer({required this.session});
  final SessionRow session;

  @override
  State<_TimePlayer> createState() => _TimePlayerState();
}

class _TimePlayerState extends State<_TimePlayer> {
  Timer? _timer;
  bool _running = false;
  int _elapsed = 0; // seconds

  int get _total => widget.session.metric <= 0 ? 60 : widget.session.metric;

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  void _start() {
    setState(() => _running = true);
    _timer = Timer.periodic(const Duration(seconds: 1), (_) {
      setState(() => _elapsed++);
      if (_elapsed >= _total) {
        _timer?.cancel();
        setState(() => _running = false);
        showFeedbackSheet(context, summary: 'Workout complete');
      }
    });
  }

  void _pause() {
    _timer?.cancel();
    setState(() => _running = false);
  }

  void _reset() {
    _timer?.cancel();
    setState(() {
      _running = false;
      _elapsed = 0;
    });
  }

  @override
  Widget build(BuildContext context) {
    final remaining = (_total - _elapsed).clamp(0, _total);
    return _PlayerScaffold(
      title: widget.session.title,
      // ignore: sort_child_properties_last — child is the dial, controls trail it.
      child: _Dial(
        progress: _elapsed / _total,
        urgent: remaining <= 10 && _running,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
              _fmtSec(remaining),
              key: const Key('player-time-remaining'),
              style: TextStyle(
                fontSize: 56,
                fontWeight: FontWeight.w600,
                fontFeatures: const [FontFeature.tabularFigures()],
                color: AtletTokens.ink,
              ),
            ),
            Text('of ${_fmtSec(_total)}',
                style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body)),
          ],
        ),
      ),
      controls: _TransportControls(
        running: _running,
        started: _elapsed > 0,
        onStart: _start,
        onPause: _pause,
        onReset: _reset,
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Reps
// ---------------------------------------------------------------------------

class _RepsPlayer extends StatefulWidget {
  const _RepsPlayer({required this.session});
  final SessionRow session;

  @override
  State<_RepsPlayer> createState() => _RepsPlayerState();
}

class _RepsPlayerState extends State<_RepsPlayer> {
  int _done = 0;

  int get _target => widget.session.metric <= 0 ? 10 : widget.session.metric;

  void _bump(int d) {
    setState(() => _done = (_done + d).clamp(0, _target));
    if (_done >= _target) {
      showFeedbackSheet(context, summary: '$_target reps done');
    }
  }

  @override
  Widget build(BuildContext context) {
    return _PlayerScaffold(
      title: widget.session.title,
      // ignore: sort_child_properties_last — child is the dial, controls trail it.
      child: _Dial(
        progress: _done / _target,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
              '$_done',
              key: const Key('player-reps-count'),
              style: TextStyle(
                  fontSize: 64, fontWeight: FontWeight.w600, color: AtletTokens.ink),
            ),
            Text('of $_target reps',
                style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body)),
          ],
        ),
      ),
      controls: Row(
        children: [
          Expanded(
            child: OutlinedButton(
              key: const Key('player-rep-minus'),
              onPressed: _done > 0 ? () => _bump(-1) : null,
              child: const Text('-1'),
            ),
          ),
          const SizedBox(width: 12),
          Expanded(
            flex: 2,
            child: FilledButton(
              key: const Key('player-rep-plus'),
              onPressed: _done < _target ? () => _bump(1) : null,
              style: FilledButton.styleFrom(
                backgroundColor: AtletTokens.accent,
                padding: const EdgeInsets.symmetric(vertical: 14),
              ),
              child: const Text('+1 rep'),
            ),
          ),
        ],
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Distance
// ---------------------------------------------------------------------------

class _DistancePlayer extends StatefulWidget {
  const _DistancePlayer({required this.session});
  final SessionRow session;

  @override
  State<_DistancePlayer> createState() => _DistancePlayerState();
}

class _DistancePlayerState extends State<_DistancePlayer> {
  Timer? _timer;
  bool _running = false;
  int _elapsed = 0;

  int get _targetKm => widget.session.metric;

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  void _toggle() {
    if (_running) {
      _timer?.cancel();
      setState(() => _running = false);
    } else {
      setState(() => _running = true);
      _timer = Timer.periodic(
          const Duration(seconds: 1), (_) => setState(() => _elapsed++));
    }
  }

  void _finish() {
    _timer?.cancel();
    setState(() => _running = false);
    showFeedbackSheet(context,
        summary: '$_targetKm km in ${_fmtSec(_elapsed)}');
  }

  @override
  Widget build(BuildContext context) {
    return _PlayerScaffold(
      title: widget.session.title,
      // ignore: sort_child_properties_last
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text('$_targetKm km target',
              style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body)),
          const SizedBox(height: 8),
          Text(
            _fmtSec(_elapsed),
            key: const Key('player-distance-elapsed'),
            style: TextStyle(
              fontSize: 64,
              fontWeight: FontWeight.w600,
              fontFeatures: const [FontFeature.tabularFigures()],
              color: AtletTokens.ink,
            ),
          ),
        ],
      ),
      controls: Column(
        children: [
          FilledButton(
            key: const Key('player-distance-toggle'),
            onPressed: _toggle,
            style: FilledButton.styleFrom(
              backgroundColor: AtletTokens.accent,
              minimumSize: const Size.fromHeight(48),
            ),
            child: Text(_running ? 'Pause' : (_elapsed > 0 ? 'Resume' : 'Start')),
          ),
          const SizedBox(height: 12),
          TextButton(
            key: const Key('player-distance-finish'),
            onPressed: _elapsed > 0 ? _finish : null,
            child: const Text('Finish run'),
          ),
        ],
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Transport controls (time player)
// ---------------------------------------------------------------------------

class _TransportControls extends StatelessWidget {
  const _TransportControls({
    required this.running,
    required this.started,
    required this.onStart,
    required this.onPause,
    required this.onReset,
  });

  final bool running;
  final bool started;
  final VoidCallback onStart;
  final VoidCallback onPause;
  final VoidCallback onReset;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        FilledButton(
          key: const Key('player-toggle'),
          onPressed: running ? onPause : onStart,
          style: FilledButton.styleFrom(
            backgroundColor: AtletTokens.accent,
            minimumSize: const Size.fromHeight(48),
          ),
          child: Text(running ? 'Pause' : (started ? 'Resume' : 'Start')),
        ),
        const SizedBox(height: 12),
        TextButton(
          key: const Key('player-reset'),
          onPressed: started ? onReset : null,
          child: const Text('Reset'),
        ),
      ],
    );
  }
}

// ---------------------------------------------------------------------------
// Feedback sheet — design FEEDBACK_OPTIONS + FEEDBACK_TAGS
// ---------------------------------------------------------------------------

const _feedbackOptions = [
  ('😮‍💨', 'Tough'),
  ('🙂', 'Solid'),
  ('💪', 'Strong'),
  ('🔥', 'Crushed it'),
];

const _feedbackTags = [
  'Felt strong', 'Short on time', 'Sore', 'Good form',
  'Crushed it', 'Low energy', 'New PR',
];

/// Shows the post-workout feedback sheet. Display-only: the adapter exposes
/// no feedback persistence surface, so submit acknowledges and pops back to
/// the session detail. Named honestly (design task-12 precedent).
Future<void> showFeedbackSheet(BuildContext context, {required String summary}) {
  return showModalBottomSheet<void>(
    context: context,
    isScrollControlled: true,
    backgroundColor: AtletTokens.bone,
    builder: (sheetContext) => _FeedbackSheet(summary: summary),
  );
}

class _FeedbackSheet extends StatefulWidget {
  const _FeedbackSheet({required this.summary});
  final String summary;

  @override
  State<_FeedbackSheet> createState() => _FeedbackSheetState();
}

class _FeedbackSheetState extends State<_FeedbackSheet> {
  int? _rating;
  final Set<String> _tags = {};

  void _close() {
    // Pop the sheet, then the player — back to session detail.
    Navigator.of(context).pop();
  }

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('How did it go?',
                style: TextStyle(
                    fontSize: AtletTokens.title2,
                    fontWeight: FontWeight.w600,
                    color: AtletTokens.ink)),
            const SizedBox(height: 4),
            Text(widget.summary,
                style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body)),
            const SizedBox(height: 16),
            Row(
              children: [
                for (final (i, opt) in _feedbackOptions.indexed) ...[
                  if (i > 0) const SizedBox(width: 8),
                  Expanded(
                    child: _RatingChip(
                      emoji: opt.$1,
                      label: opt.$2,
                      selected: _rating == i,
                      onTap: () => setState(() => _rating = i),
                    ),
                  ),
                ],
              ],
            ),
            const SizedBox(height: 16),
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                for (final tag in _feedbackTags)
                  FilterChip(
                    label: Text(tag),
                    selected: _tags.contains(tag),
                    selectedColor: AtletTokens.accent.withValues(alpha: 0.15),
                    onSelected: (v) => setState(() {
                      v ? _tags.add(tag) : _tags.remove(tag);
                    }),
                  ),
              ],
            ),
            const SizedBox(height: 20),
            FilledButton(
              key: const Key('feedback-done'),
              onPressed: _close,
              style: FilledButton.styleFrom(
                backgroundColor: AtletTokens.accent,
                minimumSize: const Size.fromHeight(48),
              ),
              child: const Text('Done'),
            ),
          ],
        ),
      ),
    );
  }
}

class _RatingChip extends StatelessWidget {
  const _RatingChip({
    required this.emoji,
    required this.label,
    required this.selected,
    required this.onTap,
  });

  final String emoji;
  final String label;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return InkWell(
      onTap: onTap,
      borderRadius: BorderRadius.circular(12),
      child: Container(
        padding: const EdgeInsets.symmetric(vertical: 10),
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(12),
          border: Border.all(
            color: selected ? AtletTokens.accent : AtletTokens.ink.withValues(alpha: 0.15),
            width: selected ? 2 : 1,
          ),
        ),
        child: Column(
          children: [
            Text(emoji, style: const TextStyle(fontSize: 22)),
            const SizedBox(height: 2),
            Text(label,
                style: TextStyle(fontSize: 11, color: AtletTokens.ink3),
                maxLines: 1,
                overflow: TextOverflow.ellipsis),
          ],
        ),
      ),
    );
  }
}
