import 'dart:async';
import 'dart:io';

import '../adapters/sync_adapter.dart';

/// Linear-interpolation percentile between closest ranks (numpy default /
/// Excel PERCENTILE.INC). Deterministic and matches known test vectors, e.g.
/// `percentile(1..100, 95) == 95.05`. Both engines under comparison must use
/// this same method or their numbers aren't comparable.
double percentile(List<num> values, num p) {
  if (values.isEmpty) {
    throw ArgumentError('percentile: values must not be empty');
  }
  if (p < 0 || p > 100) {
    throw ArgumentError.value(p, 'p', 'must be between 0 and 100');
  }
  final sorted = [...values]..sort();
  if (sorted.length == 1) return sorted.first.toDouble();
  final rank = (p / 100) * (sorted.length - 1);
  final lowIndex = rank.floor();
  final highIndex = rank.ceil();
  if (lowIndex == highIndex) return sorted[lowIndex].toDouble();
  final fraction = rank - lowIndex;
  return (sorted[lowIndex] +
          (sorted[highIndex] - sorted[lowIndex]) * fraction)
      .toDouble();
}

double median(List<num> values) => percentile(values, 50);

double p95(List<num> values) => percentile(values, 95);

/// One bench run's result, shaped to match the `analytics_runs` row
/// (apps/atlet/supabase/migrations/0001_atlet_schema.sql: sdk, engine,
/// profile, run_type, spec_version, device jsonb, metrics jsonb, started_at).
/// `seed_size`/`app_version` have no dedicated columns so they ride inside
/// `device`; `metrics` is always stamped with [evaluationLabel] per
/// spec/metrics.md ("Every run records ... Label everywhere").
class RunRecord {
  static const String evaluationLabel =
      'Internal evaluation — not a published benchmark';

  final String sdk;
  final String engine; // 'cairn' | 'powersync'
  final String profile; // 'local' | 'cloud'
  final String runType; // 'cold_sync' | 'propagation' | 'write_ack' | 'queue_drain'
  final String specVersion;
  final int seedSize;
  final String appVersion;
  final Map<String, dynamic> device; // {model, os}
  final Map<String, dynamic> metrics; // scenario-specific numbers
  final DateTime startedAt;

  const RunRecord({
    required this.sdk,
    required this.engine,
    required this.profile,
    required this.runType,
    required this.specVersion,
    required this.seedSize,
    required this.appVersion,
    required this.device,
    required this.metrics,
    required this.startedAt,
  });

  Map<String, dynamic> toJson() => {
    'sdk': sdk,
    'engine': engine,
    'profile': profile,
    'run_type': runType,
    'spec_version': specVersion,
    'device': {...device, 'seed_size': seedSize, 'app_version': appVersion},
    'metrics': {...metrics, 'label': evaluationLabel},
    'started_at': startedAt.toUtc().toIso8601String(),
  };

  factory RunRecord.fromJson(Map<String, dynamic> json) {
    final device = Map<String, dynamic>.from(json['device'] as Map);
    final metrics = Map<String, dynamic>.from(json['metrics'] as Map);
    final seedSize = device.remove('seed_size') as int;
    final appVersion = device.remove('app_version') as String;
    metrics.remove('label');
    return RunRecord(
      sdk: json['sdk'] as String,
      engine: json['engine'] as String,
      profile: json['profile'] as String,
      runType: json['run_type'] as String,
      specVersion: json['spec_version'] as String,
      seedSize: seedSize,
      appVersion: appVersion,
      device: device,
      metrics: metrics,
      startedAt: DateTime.parse(json['started_at'] as String),
    );
  }
}

/// Runs the core-4 bench scenarios (spec/metrics.md) against any
/// [SyncAdapter] — never a concrete engine type. Marks are consumed via
/// `adapter.marks`, which the harness subscribes to *before* triggering the
/// action that produces them: `SyncAdapter.marks` is a broadcast stream with
/// no replay, so subscribing after `addSession`/`setConnected` risks losing
/// marks that already fired and hanging until [timeout].
class Runner {
  Runner({
    required this.sdk,
    required this.engine,
    required this.profile,
    required this.specVersion,
    required this.seedSize,
    required this.appVersion,
    required this.device,
    Stopwatch? clock,
  }) : clock = clock ?? (Stopwatch()..start());

  final String sdk;
  final String engine;
  final String profile;
  final String specVersion;
  final int seedSize;
  final String appVersion;
  final Map<String, dynamic> device;

  /// Shared monotonic clock. `SyncMark.tMono` (sync_adapter.dart) is
  /// documented as sourced "from bench clock" — real adapters are expected
  /// to stamp marks against this same [Stopwatch] instance so `tMono` deltas
  /// between Runner and adapter are comparable.
  final Stopwatch clock;

  RunRecord _record(
    String runType,
    Map<String, dynamic> metrics,
    DateTime startedAt,
  ) => RunRecord(
    sdk: sdk,
    engine: engine,
    profile: profile,
    runType: runType,
    specVersion: specVersion,
    seedSize: seedSize,
    appVersion: appVersion,
    device: device,
    metrics: metrics,
    startedAt: startedAt,
  );

  /// cold_sync_ms: init on a wiped device until watchSessions first emits
  /// exactly [seedSize] rows. The remote must already be seeded with
  /// [seedSize] rows before calling this — Runner doesn't seed data itself,
  /// it only measures the client's catch-up.
  Future<RunRecord> coldSync(
    SyncAdapter adapter, {
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
    Duration timeout = const Duration(seconds: 60),
  }) async {
    final startedAt = DateTime.now().toUtc();
    final sw = Stopwatch()..start();
    final completer = Completer<Duration>();
    final sub = adapter.watchSessions().listen((rows) {
      if (!completer.isCompleted && rows.length == seedSize) {
        completer.complete(sw.elapsed);
      }
    });
    try {
      await adapter.init(
        supabaseUrl: supabaseUrl,
        accessToken: accessToken,
        userId: userId,
        dbDir: dbDir,
      );
      final elapsed = await completer.future.timeout(timeout);
      final ms = elapsed.inMilliseconds;
      return _record('cold_sync', {
        'cold_sync_ms': ms,
        if (ms > 0) 'rows_per_sec': seedSize * 1000 / ms,
      }, startedAt);
    } finally {
      await sub.cancel();
    }
  }

  /// propagation_ms (N=25, median/p95): harness inserts a row via PostgREST
  /// ([insertRemoteRow]); value = client_wall(remoteVisible) -
  /// server_committed_at - [clockOffset].
  Future<RunRecord> propagation(
    SyncAdapter adapter, {
    required Future<String> Function() insertRemoteRow,
    required Duration clockOffset,
    int n = 25,
    Duration timeout = const Duration(seconds: 60),
  }) async {
    final startedAt = DateTime.now().toUtc();
    final buffered = <String, SyncMark>{};
    final waiters = <String, Completer<SyncMark>>{};
    final sub = adapter.marks.listen((mark) {
      if (mark.kind != MarkKind.remoteVisible) return;
      final waiter = waiters.remove(mark.rowId);
      if (waiter != null && !waiter.isCompleted) {
        waiter.complete(mark);
      } else {
        buffered[mark.rowId] = mark;
      }
    });
    try {
      final samplesMs = <num>[];
      for (var i = 0; i < n; i++) {
        final rowId = await insertRemoteRow();
        final existing = buffered.remove(rowId);
        final SyncMark mark;
        if (existing != null) {
          mark = existing;
        } else {
          final completer = Completer<SyncMark>();
          waiters[rowId] = completer;
          mark = await completer.future.timeout(timeout);
        }
        final serverCommittedAt = mark.serverCommittedAt;
        if (serverCommittedAt == null) {
          throw StateError(
            'remoteVisible mark for $rowId missing serverCommittedAt',
          );
        }
        final clientWall = DateTime.now().toUtc();
        // clockOffset = serverNow - clientMidRtt (clock.dart), positive when
        // the server clock is ahead. A raw client_wall - server_committed_at
        // reading is therefore true_delay - clockOffset (the client reads
        // behind by exactly that skew), so recovering true_delay requires
        // ADDING the offset back, not subtracting it. Verified: true delay
        // 500ms, offset 120ms (server ahead) -> raw diff 380ms -> +120 =
        // 500ms (correct). Subtracting gives 260ms (wrong by 2x offset).
        final ms =
            clientWall.difference(serverCommittedAt).inMilliseconds +
            clockOffset.inMilliseconds;
        samplesMs.add(ms);
      }
      return _record('propagation', {
        'propagation_ms_median': median(samplesMs),
        'propagation_ms_p95': p95(samplesMs),
        'n': n,
        'samples_ms': samplesMs,
      }, startedAt);
    } finally {
      await sub.cancel();
    }
  }

  /// write_ack_ms (N=25, median/p95): tMono(serverAcked) - tMono(addSession).
  Future<RunRecord> writeAck(
    SyncAdapter adapter, {
    required SessionRow Function(int i) buildSession,
    int n = 25,
    Duration timeout = const Duration(seconds: 60),
  }) async {
    final startedAt = DateTime.now().toUtc();
    final buffered = <String, SyncMark>{};
    final waiters = <String, Completer<SyncMark>>{};
    final sub = adapter.marks.listen((mark) {
      if (mark.kind != MarkKind.serverAcked) return;
      final waiter = waiters.remove(mark.rowId);
      if (waiter != null && !waiter.isCompleted) {
        waiter.complete(mark);
      } else {
        buffered[mark.rowId] = mark;
      }
    });
    try {
      final samplesMs = <num>[];
      for (var i = 0; i < n; i++) {
        final tSend = clock.elapsed;
        final id = await adapter.addSession(buildSession(i));
        final existing = buffered.remove(id);
        final SyncMark mark;
        if (existing != null) {
          mark = existing;
        } else {
          final completer = Completer<SyncMark>();
          waiters[id] = completer;
          mark = await completer.future.timeout(timeout);
        }
        samplesMs.add((mark.tMono - tSend).inMilliseconds);
      }
      return _record('write_ack', {
        'write_ack_ms_median': median(samplesMs),
        'write_ack_ms_p95': p95(samplesMs),
        'n': n,
        'samples_ms': samplesMs,
      }, startedAt);
    } finally {
      await sub.cancel();
    }
  }

  /// queue_drain_ms: setConnected(false), [n] writes, setConnected(true);
  /// value = last serverAcked tMono - reconnect tMono.
  Future<RunRecord> queueDrain(
    SyncAdapter adapter, {
    required SessionRow Function(int i) buildSession,
    int n = 25,
    Duration timeout = const Duration(seconds: 60),
  }) async {
    final startedAt = DateTime.now().toUtc();
    final acked = <String, Duration>{};
    final expectedIds = <String>{};
    final drained = Completer<void>();
    final sub = adapter.marks.listen((mark) {
      if (mark.kind != MarkKind.serverAcked) return;
      acked[mark.rowId] = mark.tMono;
      if (expectedIds.length == n &&
          acked.length >= expectedIds.length &&
          !drained.isCompleted) {
        drained.complete();
      }
    });
    try {
      await adapter.setConnected(false);
      for (var i = 0; i < n; i++) {
        final id = await adapter.addSession(buildSession(i));
        expectedIds.add(id);
      }
      // A compliant adapter acks nothing while offline (spec/adapter.md
      // conformance #3), so `acked` should already be empty here. Clear it
      // anyway so a non-compliant adapter's premature acks can't leak into
      // the post-reconnect measurement; completion then waits strictly for
      // fresh acks, and a non-compliant adapter that never re-acks times
      // out loudly instead of reporting a fabricated (possibly negative)
      // duration.
      acked.clear();
      final reconnectAt = clock.elapsed;
      await adapter.setConnected(true);
      await drained.future.timeout(timeout);
      final lastAckTMono = acked.values.reduce((a, b) => a > b ? a : b);
      if (lastAckTMono < reconnectAt) {
        throw StateError(
          'queueDrain: serverAcked mark ($lastAckTMono) predates reconnect '
          '($reconnectAt) — adapter acked while offline',
        );
      }
      return _record('queue_drain', {
        'queue_drain_ms': (lastAckTMono - reconnectAt).inMilliseconds,
        'n': n,
      }, startedAt);
    } finally {
      await sub.cancel();
    }
  }

  /// db_bytes (spec/metrics.md item 5): sums every regular file under
  /// [dbDir], recursively. Deliberately does NOT hard-code an engine-specific
  /// filename (`cairn.sqlite` vs `powersync.db`) — Runner never imports a
  /// concrete adapter type (see class doc), and summing the whole directory
  /// keeps this method correct for either engine's on-disk footprint,
  /// including WAL/SHM/journal sidecars. Best effort per spec: the caller is
  /// expected to invoke this only after a cold sync and a full queue-drain
  /// have put both engines in a comparable checkpoint state. `journal_mode`
  /// is inferred (best effort, not queried via PRAGMA — SyncAdapter exposes
  /// no raw-SQL escape hatch) from the presence of a `-wal` sidecar file,
  /// which sqlite's default WAL journal mode leaves behind.
  Future<RunRecord> dbBytes(String dbDir) async {
    final startedAt = DateTime.now().toUtc();
    final dir = Directory(dbDir);
    var totalBytes = 0;
    var sawWalFile = false;
    if (await dir.exists()) {
      await for (final entity in dir.list(recursive: true)) {
        if (entity is File) {
          totalBytes += await entity.length();
          if (entity.path.endsWith('-wal')) sawWalFile = true;
        }
      }
    }
    return _record('db_bytes', {
      'db_bytes': totalBytes,
      'journal_mode': sawWalFile ? 'wal' : 'unknown',
    }, startedAt);
  }
}
