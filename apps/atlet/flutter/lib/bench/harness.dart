import 'dart:async';
import 'dart:io';

import 'package:supabase_flutter/supabase_flutter.dart';

import '../adapters/sync_adapter.dart';
import '../engine_registry.dart';
import 'runner.dart';
import 'store.dart';

/// Pure, testable payload shaper for the propagation run's PostgREST insert
/// (task-14 brief: harness inserts via `supabase.from('sessions').insert(...)`
/// using the signed-in user's REST session — NOT through either engine).
/// Omits `id`/`user_id`: 0001_atlet_schema.sql defaults both
/// (`gen_random_uuid()` / `auth.uid()`) server-side. Mirrors the shape of
/// nostos_adapter.dart/powersync_adapter.dart's own `sessionWritePayload`,
/// minus the client-generated `id` those need for local-first upserts —
/// this path lets Postgres assign it and reads it back instead.
Map<String, dynamic> sessionInsertPayload(SessionRow s) => {
  'title': s.title,
  'type': s.type,
  'metric': s.metric,
  'unit': s.unit,
  if (s.note != null) 'note': s.note,
  'streak': s.streak,
  'occurred_on': _dateOnly(s.occurredOn),
};

String _dateOnly(DateTime d) =>
    '${d.year.toString().padLeft(4, '0')}-'
    '${d.month.toString().padLeft(2, '0')}-'
    '${d.day.toString().padLeft(2, '0')}';

/// Builds the `insertRemoteRow` closure [Runner.propagation] needs, wired to
/// a live [SupabaseClient]'s PostgREST session (production path). Not
/// exercised by a live network call in this task's test suite — no
/// Supabase/docker in this environment, see task-14-report.md — but
/// [sessionInsertPayload], which carries all of the row-shaping logic, is
/// fully unit tested.
Future<String> Function() supabasePostgrestInsert(
  SupabaseClient client, {
  required SessionRow Function(int i) buildRow,
}) {
  var callCount = 0;
  return () async {
    final row = buildRow(callCount++);
    final inserted = await client
        .from('sessions')
        .insert(sessionInsertPayload(row))
        .select('id')
        .single();
    return inserted['id'] as String;
  };
}

/// Orchestrates one full Core-4 + storage bench suite (spec/metrics.md)
/// against a single [SyncAdapter], persisting every [RunRecord] to [store]
/// as soon as it's produced (task-14 brief: "each run ends with
/// `BenchStore.append`") — so a suite that dies partway through (timeout,
/// crash) still leaves the runs that did complete on disk, rather than
/// losing everything to an all-or-nothing batch write.
///
/// Run order matters: db_bytes is measured LAST, after coldSync,
/// propagation, writeAck, and queueDrain have all completed — so "after
/// cold sync + full drain, same checkpoint state" (spec/metrics.md item 5)
/// holds.
class BenchHarness {
  BenchHarness({
    required this.runner,
    required this.adapter,
    required this.store,
    required this.supabaseUrl,
    required this.accessToken,
    required this.userId,
    required this.dbDir,
    required this.insertRemoteRow,
    required this.buildSession,
    this.clockOffset = Duration.zero,
    this.n = 25,
    this.timeout = const Duration(seconds: 60),
  });

  final Runner runner;
  final SyncAdapter adapter;
  final BenchStore store;
  final String supabaseUrl;
  final String accessToken;
  final String userId;
  final String dbDir;

  /// Inserts one row via PostgREST and returns its id — see
  /// [supabasePostgrestInsert] for the production wiring. Injectable so
  /// tests never need a live Supabase session (mirrors
  /// `Runner.propagation`'s own `insertRemoteRow` parameter).
  final Future<String> Function() insertRemoteRow;

  /// Builds the Nth session row for writeAck/queueDrain (mirrors
  /// `Runner.writeAck`/`queueDrain`'s own `buildSession` parameter).
  final SessionRow Function(int i) buildSession;

  final Duration clockOffset;
  final int n;
  final Duration timeout;

  /// Runs cold_sync, propagation, write_ack, queue_drain, then db_bytes, in
  /// that order, against [adapter] — appending each [RunRecord] to [store]
  /// as it completes. Returns all five records (e.g. for a summary print);
  /// callers that only care about persistence can ignore the return value.
  Future<List<RunRecord>> runFullSuite() async {
    final records = <RunRecord>[];

    Future<void> run(Future<RunRecord> Function() action) async {
      final record = await action();
      await store.append(record);
      records.add(record);
    }

    await run(
      () => runner.coldSync(
        adapter,
        supabaseUrl: supabaseUrl,
        accessToken: accessToken,
        userId: userId,
        dbDir: dbDir,
        timeout: timeout,
      ),
    );

    await run(
      () => runner.propagation(
        adapter,
        insertRemoteRow: insertRemoteRow,
        clockOffset: clockOffset,
        n: n,
        timeout: timeout,
      ),
    );

    await run(
      () => runner.writeAck(
        adapter,
        buildSession: buildSession,
        n: n,
        timeout: timeout,
      ),
    );

    await run(
      () => runner.queueDrain(
        adapter,
        buildSession: buildSession,
        n: n,
        timeout: timeout,
      ),
    );

    await run(() => runner.dbBytes(dbDir));

    return records;
  }
}

/// Runs one full [BenchHarness] suite per engine (task-14 brief: "Run one
/// full suite per engine on the local profile") — the production entry
/// point once a debug trigger wires it up. Each engine gets its own
/// subdirectory of [rootDbDir] so a cold sync always starts against a
/// directory with no prior engine's files sitting in it, and its own
/// [Runner] (fresh Stopwatch) so tMono deltas within one engine's Core-4
/// runs are never contaminated by wall-clock time spent running the other
/// engine's suite. [SyncAdapter.signOut] runs after each engine's suite
/// (full wipe, ADR-0029) so back-to-back suite runs never leave a live
/// session behind for the next one to collide with.
///
/// Deliberately does not go through [EngineRegistry]: that registry exists
/// to enforce "only one live" for the app's runtime UX (decision #4), but a
/// bench run needs a signOut() after EVERY suite (not just before the next
/// start()) and two independent dbDirs, neither of which
/// `EngineRegistry.switchTo`/`start`'s contract provides.
Future<Map<Engine, List<RunRecord>>> runFullSuiteForBothEngines({
  required String sdk,
  required String specVersion,
  required int seedSize,
  required String appVersion,
  required Map<String, dynamic> device,
  required String rootDbDir,
  required String supabaseUrl,
  required String accessToken,
  required String userId,
  required BenchStore store,
  required Future<String> Function() insertRemoteRow,
  required SessionRow Function(int i) buildSession,
  required Map<Engine, SyncAdapter Function()> adapterFactories,
  Duration clockOffset = Duration.zero,
  int n = 25,
  Duration timeout = const Duration(seconds: 60),
}) async {
  final results = <Engine, List<RunRecord>>{};
  for (final entry in adapterFactories.entries) {
    final engine = entry.key;
    final adapter = entry.value();
    final engineDbDir = '$rootDbDir/${engine.name}';
    await Directory(engineDbDir).create(recursive: true);
    final runner = Runner(
      sdk: sdk,
      engine: engine.name,
      profile: 'local',
      specVersion: specVersion,
      seedSize: seedSize,
      appVersion: appVersion,
      device: device,
    );
    final harness = BenchHarness(
      runner: runner,
      adapter: adapter,
      store: store,
      supabaseUrl: supabaseUrl,
      accessToken: accessToken,
      userId: userId,
      dbDir: engineDbDir,
      insertRemoteRow: insertRemoteRow,
      buildSession: buildSession,
      clockOffset: clockOffset,
      n: n,
      timeout: timeout,
    );
    // signOut in `finally`: a mid-suite failure (e.g. a benchmark timeout)
    // must not leak a live adapter — a leaked client keeps replaying its
    // outbox against the server forever (observed as repeating rejected
    // upserts after a write_ack timeout).
    try {
      results[engine] = await harness.runFullSuite();
    } finally {
      await adapter.signOut();
    }
  }
  return results;
}
