import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';
import 'player.dart';

/// Display-transform per apps/atlet/design/views/train.hints.json — kept as a
/// private copy (not shared with home.dart) to avoid a circular import
/// between the two ui/ files; the two files were built independently against
/// the same design spec, not against each other.
(String, String, String) _displayFor(SessionRow s) {
  final typeLabel = s.type.isEmpty
      ? s.type
      : s.type[0].toUpperCase() + s.type.substring(1);
  final value = s.type == 'time' ? (s.metric / 60).round() : s.metric;
  final unitLabel = switch (s.type) {
    'distance' => 'km',
    'reps' => 'reps total',
    'time' => 'minutes',
    _ => s.unit,
  };
  return (typeLabel, '$value', unitLabel);
}

/// Session detail: title, type, metric/unit (display-transformed), note,
/// streak, occurredOn, plus Complete/Delete actions. Rendered exclusively
/// from [SyncAdapter.watchSessions] — no snapshot is taken at push time — so
/// if the session is deleted (from here or elsewhere) this screen reflects
/// that the moment the adapter re-emits, and pops itself.
///
/// [SyncAdapter] exposes no update/complete verb and no "done" field on
/// [SessionRow] — both actions call the only mutator available,
/// [SyncAdapter.deleteSession]. Team-lead ruling (task-12): this is
/// "log-and-clear," not "save a done state" — Complete's snackbar names the
/// actual effect (removal) explicitly rather than implying persistence.
/// Delete keeps the destructive framing: confirmation dialog first, no
/// snackbar after. A real completed-state (sessions.completed_at + an
/// adapter update surface) is a parked, operator-gated future task — see
/// task-12-report.md Concerns.
class SessionDetail extends StatelessWidget {
  const SessionDetail({super.key, required this.adapter, required this.sessionId});

  final SyncAdapter adapter;
  final String sessionId;

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<List<SessionRow>>(
      stream: adapter.watchSessions(),
      builder: (context, snapshot) {
        if (!snapshot.hasData) {
          // Stream hasn't emitted yet — unknown, not absent. Popping here
          // would bounce straight back out before the first snapshot arrives.
          return Scaffold(backgroundColor: AtletTokens.bone, body: const SizedBox.shrink());
        }

        SessionRow? session;
        for (final s in snapshot.data!) {
          if (s.id == sessionId) session = s;
        }

        if (session == null) {
          // The stream has emitted at least once and this id isn't in it —
          // genuinely removed (by this screen's own Complete/Delete, or
          // elsewhere).
          WidgetsBinding.instance.addPostFrameCallback((_) {
            if (Navigator.of(context).canPop()) Navigator.of(context).pop();
          });
          return Scaffold(
            backgroundColor: AtletTokens.bone,
            body: const SizedBox.shrink(),
          );
        }

        final (typeLabel, valueText, unitLabel) = _displayFor(session);
        return Scaffold(
          backgroundColor: AtletTokens.bone,
          appBar: AppBar(
            backgroundColor: AtletTokens.bone,
            elevation: 0,
            title: Text(session.title, style: TextStyle(color: AtletTokens.ink)),
          ),
          body: Padding(
            padding: const EdgeInsets.all(24),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  typeLabel,
                  style: TextStyle(
                    fontSize: AtletTokens.footnote,
                    letterSpacing: 1.5,
                    color: AtletTokens.ink3,
                    fontWeight: FontWeight.w500,
                  ),
                ),
                const SizedBox(height: 8),
                Text(
                  '$valueText $unitLabel',
                  style: TextStyle(
                    fontSize: AtletTokens.largeTitle,
                    fontWeight: FontWeight.w600,
                    color: AtletTokens.ink,
                  ),
                ),
                const SizedBox(height: 16),
                Row(
                  children: [
                    Icon(Icons.local_fire_department_outlined, size: 16, color: AtletTokens.warn),
                    const SizedBox(width: 6),
                    Text('${session.streak}-day streak',
                        style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body)),
                  ],
                ),
                if (session.note != null) ...[
                  const SizedBox(height: 16),
                  Text(session.note!,
                      style: TextStyle(color: AtletTokens.ink, fontSize: AtletTokens.body)),
                ],
                const Spacer(),
                FilledButton(
                  key: const Key('start-workout-button'),
                  onPressed: () => Navigator.of(context).push(
                    MaterialPageRoute<void>(
                      builder: (_) => WorkoutPlayer(session: session!),
                    ),
                  ),
                  style: FilledButton.styleFrom(
                    backgroundColor: AtletTokens.ink,
                    padding: const EdgeInsets.symmetric(vertical: 14),
                  ),
                  child: const Text('Start workout'),
                ),
                const SizedBox(height: 12),
                FilledButton(
                  key: const Key('complete-session-button'),
                  onPressed: () => _complete(context, session!),
                  style: FilledButton.styleFrom(
                    backgroundColor: AtletTokens.accent,
                    padding: const EdgeInsets.symmetric(vertical: 14),
                  ),
                  child: const Text('Complete'),
                ),
                const SizedBox(height: 12),
                TextButton(
                  key: const Key('delete-session-button'),
                  onPressed: () => _confirmDelete(context, session!),
                  child: Text('Delete', style: TextStyle(color: AtletTokens.accent2)),
                ),
              ],
            ),
          ),
        );
      },
    );
  }

  Future<void> _complete(BuildContext context, SessionRow session) async {
    await adapter.deleteSession(session.id);
    if (context.mounted) {
      // "Log-and-clear," not "saved as done": SyncAdapter has no completion
      // field to persist, so the snackbar names the actual effect (removal
      // from the list) rather than implying state was recorded.
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('Logged — session cleared from list')),
      );
    }
  }

  Future<void> _confirmDelete(BuildContext context, SessionRow session) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Delete this session?'),
        content: const Text("This can't be undone."),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          TextButton(
            key: const Key('confirm-delete-button'),
            onPressed: () => Navigator.of(context).pop(true),
            child: Text('Delete', style: TextStyle(color: AtletTokens.accent2)),
          ),
        ],
      ),
    );
    if (confirmed == true) {
      await adapter.deleteSession(session.id);
    }
  }
}
