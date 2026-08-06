import 'package:supabase_flutter/supabase_flutter.dart';

import 'runner.dart';
import 'store.dart';

/// Signature for posting already-shaped `analytics_runs` rows to Supabase.
/// Injectable so [AnalyticsScreen] (lib/ui/analytics.dart) never needs a
/// live [SupabaseClient] in widget tests — mirrors harness.dart's
/// `insertRemoteRow` injection pattern.
typedef RunsUploader = Future<void> Function(List<Map<String, dynamic>> rows);

/// Builds a [RunsUploader] wired to a live [SupabaseClient]'s PostgREST
/// session (production path). A plain REST insert into `analytics_runs` on
/// the signed-in user's own session — analytics data never flows through
/// either sync engine under test (decision #5). [RunRecord.toJson] already
/// matches `analytics_runs`' row shape column-for-column
/// (0001_atlet_schema.sql) other than `id`/`user_id`/`uploaded_at`, which
/// Postgres assigns via defaults, so no reshaping happens here.
///
/// Not exercised by a live network call in this task's test suite — no
/// Supabase/docker in this environment (see task-14-report.md); the pure
/// row-shaping ([RunRecord.toJson], already covered by runner_test.dart)
/// and the upload orchestration ([uploadStoredRuns]) are what's under test.
RunsUploader supabasePostgrestUpload(SupabaseClient client) {
  return (rows) async {
    if (rows.isEmpty) return;
    await client.from('analytics_runs').insert(rows);
  };
}

/// Reads every run persisted in [store] and uploads it via [uploadRuns].
/// Returns the number of records uploaded — 0 (with [uploadRuns] never
/// called) if the store was empty, so the UI can distinguish "nothing to
/// upload" from a failed call without a separate code path.
Future<int> uploadStoredRuns(BenchStore store, RunsUploader uploadRuns) async {
  final records = await store.readAll();
  if (records.isEmpty) return 0;
  await uploadRuns(records.map((r) => r.toJson()).toList());
  return records.length;
}
