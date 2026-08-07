import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';
import '../util/uuid.dart';
import 'detail.dart';
import 'stats_deck.dart';

/// Session types per apps/atlet/design/data_model.json's enum. Wire `unit` is
/// derived from `type` (never user-entered) so stored rows always match the
/// w1–w5 fixture shape: distance→km, reps→reps, time→sec.
const _kSessionTypes = ['distance', 'reps', 'time'];

String _wireUnitFor(String type) => switch (type) {
      'distance' => 'km',
      'reps' => 'reps',
      'time' => 'sec',
      _ => '',
    };

/// Display-transform per apps/atlet/design/views/train.hints.json — presentation
/// only, never mutates the stored row. Returns (typeLabel, valueText, unitLabel).
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

/// Training home: session list rendered exclusively from
/// [SyncAdapter.watchSessions] — there is no local cache. A write
/// ([SyncAdapter.addSession]/[deleteSession]) only becomes visible once the
/// adapter re-emits on that stream, which is the offline-first proof this
/// screen exists to demonstrate.
class TrainingHome extends StatelessWidget {
  const TrainingHome({super.key, required this.adapter});

  final SyncAdapter? adapter;

  @override
  Widget build(BuildContext context) {
    final adapter = this.adapter;
    if (adapter == null) {
      return const _EmptyState(
        message: 'No sync engine selected.\nOpen Settings to pick one.',
      );
    }
    return StreamBuilder<List<SessionRow>>(
      stream: adapter.watchSessions(),
      builder: (context, snapshot) {
        final sessions = snapshot.data ?? const <SessionRow>[];
        return Scaffold(
          backgroundColor: AtletTokens.bone,
          floatingActionButton: FloatingActionButton(
            key: const Key('add-session-button'),
            backgroundColor: AtletTokens.accent,
            onPressed: () => _openAddSheet(context, adapter),
            child: const Icon(Icons.add, color: AtletTokens.textOnAccent),
          ),
          body: sessions.isEmpty
              ? const _EmptyState(message: 'No sessions yet.\nTap + to log one.')
              : ListView.separated(
                  key: const Key('session-list'),
                  padding: const EdgeInsets.fromLTRB(16, 16, 16, 96),
                  // Index 0 is the stats deck (design home.jsx StatsDeck);
                  // sessions follow, shifted by one.
                  itemCount: sessions.length + 1,
                  separatorBuilder: (_, _) => const SizedBox(height: 12),
                  itemBuilder: (context, i) {
                    if (i == 0) return StatsDeck(sessions: sessions);
                    final session = sessions[i - 1];
                    return _SessionCard(
                      session: session,
                      onTap: () => Navigator.of(context).push(
                        MaterialPageRoute<void>(
                          builder: (_) => SessionDetail(
                            adapter: adapter,
                            sessionId: session.id,
                          ),
                        ),
                      ),
                    );
                  },
                ),
        );
      },
    );
  }

  void _openAddSheet(BuildContext context, SyncAdapter adapter) {
    showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      builder: (_) => _AddSessionSheet(adapter: adapter),
    );
  }
}

class _EmptyState extends StatelessWidget {
  const _EmptyState({required this.message});

  final String message;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: AtletTokens.bone,
      body: Center(
        child: Text(
          message,
          textAlign: TextAlign.center,
          style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body),
        ),
      ),
    );
  }
}

class _SessionCard extends StatelessWidget {
  const _SessionCard({required this.session, required this.onTap});

  final SessionRow session;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final (typeLabel, valueText, unitLabel) = _displayFor(session);
    return Material(
      color: AtletTokens.paper,
      borderRadius: BorderRadius.circular(16),
      child: InkWell(
        borderRadius: BorderRadius.circular(16),
        onTap: onTap,
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Row(
            children: [
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      session.title,
                      style: TextStyle(
                        fontSize: AtletTokens.body,
                        fontWeight: FontWeight.w600,
                        color: AtletTokens.ink,
                      ),
                    ),
                    const SizedBox(height: 4),
                    Text(
                      '$typeLabel · $valueText $unitLabel',
                      style: TextStyle(
                        fontSize: AtletTokens.footnote,
                        color: AtletTokens.ink3,
                      ),
                    ),
                  ],
                ),
              ),
              _StreakChip(streak: session.streak),
            ],
          ),
        ),
      ),
    );
  }
}

class _StreakChip extends StatelessWidget {
  const _StreakChip({required this.streak});

  final int streak;

  @override
  Widget build(BuildContext context) {
    return Container(
      key: const Key('streak-chip'),
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
      decoration: BoxDecoration(
        color: AtletTokens.bone2,
        borderRadius: BorderRadius.circular(999),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(Icons.local_fire_department_outlined, size: 14, color: AtletTokens.warn),
          const SizedBox(width: 4),
          Text(
            '$streak',
            style: TextStyle(
              fontSize: AtletTokens.footnote,
              fontWeight: FontWeight.w600,
              color: AtletTokens.ink,
            ),
          ),
        ],
      ),
    );
  }
}

class _AddSessionSheet extends StatefulWidget {
  const _AddSessionSheet({required this.adapter});

  final SyncAdapter adapter;

  @override
  State<_AddSessionSheet> createState() => _AddSessionSheetState();
}

class _AddSessionSheetState extends State<_AddSessionSheet> {
  String _type = _kSessionTypes.first;
  final _title = TextEditingController();
  final _metric = TextEditingController();
  final _note = TextEditingController();
  bool _saving = false;

  @override
  void dispose() {
    _title.dispose();
    _metric.dispose();
    _note.dispose();
    super.dispose();
  }

  /// Upper bound on the metric input. Postgres `sessions.metric` is `int4`
  /// (max 2_147_483_647) and `time` inputs are multiplied by 60, so an
  /// unchecked large entry (e.g. 999999999 minutes) overflows the column and
  /// poisons the offline outbox with a forever-rejected write. 999_999 covers
  /// any real workout (≈694 days in minutes) with 3 orders of headroom.
  static const int _maxMetricInput = 999999;

  bool get _valid {
    if (_title.text.trim().isEmpty) return false;
    final parsed = int.tryParse(_metric.text);
    return parsed != null && parsed > 0 && parsed <= _maxMetricInput;
  }

  Future<void> _submit() async {
    final parsed = int.parse(_metric.text);
    final metric = _type == 'time' ? parsed * 60 : parsed;
    setState(() => _saving = true);
    await widget.adapter.addSession(
      SessionRow(
        // sessions.id is a Postgres `uuid` — a non-UUID client id is
        // rejected by the server write-back (invalid input syntax).
        id: uuidV4(),
        title: _title.text.trim(),
        type: _type,
        metric: metric,
        unit: _wireUnitFor(_type),
        note: _note.text.trim().isEmpty ? null : _note.text.trim(),
        occurredOn: DateTime.now(),
      ),
    );
    if (mounted) Navigator.of(context).pop();
  }

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: EdgeInsets.fromLTRB(
        24,
        24,
        24,
        24 + MediaQuery.of(context).viewInsets.bottom,
      ),
      child: SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(
              'LOG SESSION',
              style: TextStyle(
                fontSize: AtletTokens.footnote,
                letterSpacing: 1.5,
                color: AtletTokens.ink3,
                fontWeight: FontWeight.w500,
              ),
            ),
            const SizedBox(height: 16),
            SegmentedButton<String>(
              key: const Key('session-type-selector'),
              segments: [
                for (final t in _kSessionTypes)
                  ButtonSegment(value: t, label: Text(t[0].toUpperCase() + t.substring(1))),
              ],
              selected: {_type},
              onSelectionChanged: (s) => setState(() => _type = s.first),
            ),
            const SizedBox(height: 16),
            TextField(
              key: const Key('session-title-field'),
              controller: _title,
              decoration: const InputDecoration(labelText: 'Title'),
              onChanged: (_) => setState(() {}),
            ),
            const SizedBox(height: 12),
            TextField(
              key: const Key('session-metric-field'),
              controller: _metric,
              keyboardType: TextInputType.number,
              decoration: InputDecoration(
                labelText: _type == 'time' ? 'Minutes' : (_type == 'distance' ? 'Km' : 'Reps'),
                errorText: (_metric.text.isNotEmpty &&
                        ((int.tryParse(_metric.text) ?? -1) <= 0 ||
                            (int.tryParse(_metric.text) ?? 0) > _maxMetricInput))
                    ? 'Enter 1–$_maxMetricInput'
                    : null,
              ),
              onChanged: (_) => setState(() {}),
            ),
            const SizedBox(height: 12),
            TextField(
              key: const Key('session-note-field'),
              controller: _note,
              decoration: const InputDecoration(labelText: 'Note (optional)'),
            ),
            const SizedBox(height: 20),
            FilledButton(
              key: const Key('save-session-button'),
              onPressed: _valid && !_saving ? _submit : null,
              style: FilledButton.styleFrom(
                backgroundColor: AtletTokens.accent,
                padding: const EdgeInsets.symmetric(vertical: 14),
              ),
              child: _saving
                  ? const SizedBox(
                      height: 18,
                      width: 18,
                      child: CircularProgressIndicator(
                        strokeWidth: 2,
                        color: AtletTokens.textOnAccent,
                      ),
                    )
                  : const Text('Save'),
            ),
          ],
        ),
      ),
    );
  }
}
