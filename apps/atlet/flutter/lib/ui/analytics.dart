import 'package:flutter/material.dart';

import '../bench/runner.dart';
import '../bench/store.dart';
import '../bench/upload.dart';
import '../design/tokens.dart';
import 'connectivity_led.dart';

/// Canonical Core-4 + storage run order (spec/metrics.md) — used to sort
/// [latestMetricRows] so the table reads top-to-bottom the same way
/// [BenchHarness.runFullSuite] produces them, regardless of JSONL append
/// order.
const _runTypeOrder = [
  'cold_sync',
  'propagation',
  'write_ack',
  'queue_drain',
  'db_bytes',
];

/// One flattened, display-ready line of the results table: which
/// engine/run_type it's for, a human label + unit, and its value(s). [p95]
/// is null for run types that only ever produce a single sample
/// (cold_sync/queue_drain/db_bytes) — the table renders an em-dash rather
/// than repeating [value] into a fake "p95".
class MetricRow {
  const MetricRow({
    required this.engine,
    required this.runType,
    required this.label,
    required this.unit,
    required this.value,
    this.p95,
  });

  final String engine;
  final String runType;
  final String label;
  final String unit;
  final num value;
  final num? p95;
}

/// Picks [RunRecord.metrics]' primary number (median where the run has one)
/// out by exact key, per [Runner]'s own field names — no generic
/// "first numeric value" guessing, which would silently mislabel whichever
/// field happens to sort first.
MetricRow metricRowFor(RunRecord r) {
  switch (r.runType) {
    case 'cold_sync':
      return MetricRow(
        engine: r.engine,
        runType: r.runType,
        label: 'Cold sync',
        unit: 'ms',
        value: r.metrics['cold_sync_ms'] as num,
      );
    case 'propagation':
      return MetricRow(
        engine: r.engine,
        runType: r.runType,
        label: 'Propagation',
        unit: 'ms',
        value: r.metrics['propagation_ms_median'] as num,
        p95: r.metrics['propagation_ms_p95'] as num,
      );
    case 'write_ack':
      return MetricRow(
        engine: r.engine,
        runType: r.runType,
        label: 'Write ack',
        unit: 'ms',
        value: r.metrics['write_ack_ms_median'] as num,
        p95: r.metrics['write_ack_ms_p95'] as num,
      );
    case 'queue_drain':
      return MetricRow(
        engine: r.engine,
        runType: r.runType,
        label: 'Queue drain',
        unit: 'ms',
        value: r.metrics['queue_drain_ms'] as num,
      );
    case 'db_bytes':
      return MetricRow(
        engine: r.engine,
        runType: r.runType,
        label: 'DB size',
        unit: 'bytes',
        value: r.metrics['db_bytes'] as num,
      );
    default:
      throw ArgumentError.value(r.runType, 'runType', 'unknown run type');
  }
}

/// Reduces a (possibly multi-run) history down to one row per
/// (engine, run_type) — the latest by [RunRecord.startedAt] — then sorts by
/// [_runTypeOrder] within each engine so re-running a suite updates the
/// table in place instead of appending duplicate rows underneath.
List<MetricRow> latestMetricRows(List<RunRecord> records) {
  final latest = <String, RunRecord>{};
  for (final r in records) {
    final key = '${r.engine}/${r.runType}';
    final existing = latest[key];
    if (existing == null || r.startedAt.isAfter(existing.startedAt)) {
      latest[key] = r;
    }
  }
  final rows = latest.values.map(metricRowFor).toList()
    ..sort((a, b) {
      final engineCmp = a.engine.compareTo(b.engine);
      if (engineCmp != 0) return engineCmp;
      return _runTypeOrder.indexOf(a.runType).compareTo(
        _runTypeOrder.indexOf(b.runType),
      );
    });
  return rows;
}

/// Tab 2 (task-15 brief): run launcher + results table + upload. Analytics
/// data never flows through either sync engine under test (decision #5) —
/// [store]/[uploadRuns]/[runSuite] are all injected so this screen never
/// needs a live [SyncAdapter] or [SupabaseClient] to be testable, mirroring
/// shop.dart/detail.dart's adapter-injection pattern. Reached via the
/// Analytics tab of main.dart's bottom nav (I-1 fix), which supplies the
/// production store/uploadRuns/runSuite wiring.
class AnalyticsScreen extends StatefulWidget {
  const AnalyticsScreen({
    super.key,
    required this.store,
    required this.uploadRuns,
    required this.runSuite,
  });

  final BenchStore store;
  final RunsUploader uploadRuns;

  /// Runs a full bench suite (production wiring: [runFullSuiteForBothEngines]
  /// against live adapters) and appends its records to [store]. Injected so
  /// widget tests never need a live [SyncAdapter] — mirrors [uploadRuns].
  final Future<void> Function() runSuite;

  @override
  State<AnalyticsScreen> createState() => _AnalyticsScreenState();
}

class _AnalyticsScreenState extends State<AnalyticsScreen> {
  List<RunRecord>? _records;
  bool _running = false;
  bool _uploading = false;
  String? _statusMessage;

  @override
  void initState() {
    super.initState();
    _reload();
  }

  Future<void> _reload() async {
    final records = await widget.store.readAll();
    if (!mounted) return;
    setState(() => _records = records);
  }

  Future<void> _runSuite() async {
    setState(() {
      _running = true;
      _statusMessage = null;
    });
    try {
      await widget.runSuite();
      await _reload();
      if (!mounted) return;
      setState(() => _statusMessage = 'Run complete.');
    } catch (e) {
      if (!mounted) return;
      setState(() => _statusMessage = 'Run failed: $e');
    } finally {
      if (mounted) setState(() => _running = false);
    }
  }

  Future<void> _upload() async {
    setState(() {
      _uploading = true;
      _statusMessage = null;
    });
    try {
      final count = await uploadStoredRuns(widget.store, widget.uploadRuns);
      if (!mounted) return;
      setState(
        () => _statusMessage = count == 0
            ? 'No runs to upload.'
            : 'Uploaded $count run(s).',
      );
    } catch (e) {
      if (!mounted) return;
      setState(() => _statusMessage = 'Upload failed: $e');
    } finally {
      if (mounted) setState(() => _uploading = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final records = _records;
    return Scaffold(
      key: const Key('analytics-screen'),
      backgroundColor: AtletTokens.paper,
      appBar: AppBar(
        title: const Text('Analytics'),
        backgroundColor: AtletTokens.bone,
        actions: const [ConnectivityLed()],
      ),
      body: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const _EvaluationBanner(),
          Padding(
            padding: const EdgeInsets.all(16),
            child: Row(
              children: [
                ElevatedButton(
                  key: const Key('run-suite-button'),
                  onPressed: _running ? null : _runSuite,
                  child: _running
                      ? const SizedBox(
                          width: 16,
                          height: 16,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : const Text('Run suite'),
                ),
                const SizedBox(width: 12),
                ElevatedButton(
                  key: const Key('upload-button'),
                  onPressed: _uploading ? null : _upload,
                  child: _uploading
                      ? const SizedBox(
                          width: 16,
                          height: 16,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : const Text('Upload'),
                ),
              ],
            ),
          ),
          if (_statusMessage != null)
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 16),
              child: Text(
                _statusMessage!,
                key: const Key('analytics-status'),
                style: const TextStyle(color: AtletTokens.ink3),
              ),
            ),
          const SizedBox(height: 8),
          Expanded(
            child: records == null
                ? const Center(child: CircularProgressIndicator())
                : records.isEmpty
                ? const Center(child: Text('No runs yet.'))
                : _ResultsTable(rows: latestMetricRows(records)),
          ),
        ],
      ),
    );
  }
}

/// Permanent internal-eval disclaimer (decision #10). Text must match
/// [RunRecord.evaluationLabel] exactly — that's the string PostgREST rows
/// carry too, so the banner and the uploaded data never disagree.
class _EvaluationBanner extends StatelessWidget {
  const _EvaluationBanner();

  @override
  Widget build(BuildContext context) {
    return Container(
      key: const Key('analytics-eval-banner'),
      width: double.infinity,
      color: AtletTokens.warn,
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
      child: Text(
        RunRecord.evaluationLabel,
        style: const TextStyle(
          color: AtletTokens.ink,
          fontWeight: FontWeight.w600,
          fontSize: AtletTokens.footnote,
        ),
      ),
    );
  }
}

class _ResultsTable extends StatelessWidget {
  const _ResultsTable({required this.rows});

  final List<MetricRow> rows;

  @override
  Widget build(BuildContext context) {
    return ListView(
      key: const Key('results-table'),
      padding: const EdgeInsets.all(16),
      children: [
        const Row(
          children: [
            Expanded(flex: 2, child: Text('Engine')),
            Expanded(flex: 3, child: Text('Metric')),
            Expanded(flex: 2, child: Text('Median')),
            Expanded(flex: 2, child: Text('p95')),
          ],
        ),
        const Divider(color: AtletTokens.rule),
        for (final row in rows)
          Padding(
            key: Key('result-row-${row.engine}-${row.runType}'),
            padding: const EdgeInsets.symmetric(vertical: 6),
            child: Row(
              children: [
                Expanded(flex: 2, child: Text(row.engine)),
                Expanded(flex: 3, child: Text(row.label)),
                Expanded(
                  flex: 2,
                  child: Text('${_fmt(row.value)} ${row.unit}'),
                ),
                Expanded(
                  flex: 2,
                  child: Text(
                    row.p95 == null ? '—' : '${_fmt(row.p95!)} ${row.unit}',
                  ),
                ),
              ],
            ),
          ),
      ],
    );
  }

  String _fmt(num n) => n is int ? '$n' : n.toStringAsFixed(1);
}
