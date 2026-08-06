import 'package:supabase_flutter/supabase_flutter.dart';

import 'runner.dart' show median;

/// One clock-offset probe sample: client wall-clock instants bracketing a
/// round trip, and the server's own clock reading returned by that trip.
class ClockProbeSample {
  final DateTime sentAt;
  final DateTime receivedAt;
  final DateTime serverNow;

  const ClockProbeSample({
    required this.sentAt,
    required this.receivedAt,
    required this.serverNow,
  });

  Duration get rtt => receivedAt.difference(sentAt);

  /// offset = serverNow - clientMidRtt (RTT/2-corrected), per
  /// spec/metrics.md: "offset = median of (server_now - client_mid_rtt)".
  Duration get offset => serverNow.difference(sentAt.add(rtt ~/ 2));
}

typedef ClockProbe = Future<ClockProbeSample> Function();

/// Estimates the wall-clock offset between this device and the Postgres
/// server via 5 RTT/2-corrected round trips (spec/metrics.md), taking the
/// median to resist a single slow/jittery sample.
class BenchClock {
  static Future<Duration> estimateOffset({
    SupabaseClient? client,
    ClockProbe? probe,
    int samples = 5,
  }) async {
    final effectiveProbe = probe ?? (() => _defaultProbe(_requireClient(client)));
    final offsetsMs = <num>[];
    for (var i = 0; i < samples; i++) {
      final sample = await effectiveProbe();
      offsetsMs.add(sample.offset.inMilliseconds);
    }
    return Duration(milliseconds: median(offsetsMs).round());
  }

  static SupabaseClient _requireClient(SupabaseClient? client) {
    if (client == null) {
      throw ArgumentError('estimateOffset requires either client or probe');
    }
    return client;
  }

  // ponytail: relies on a `bench_now()` Postgres RPC (Supabase requires
  // exposing custom functions via RPC — there is no bare "SELECT now()"
  // over PostgREST). An earlier draft used the HTTP `Date` response header
  // instead to avoid a new migration, but that header is second-resolution
  // and truncates, biasing every sample ~0-999ms low — larger than the
  // interval being measured. Ceiling: needs
  // apps/atlet/supabase/migrations/0003_bench_now_rpc.sql applied to the
  // target project before this codepath works; tests always inject `probe`
  // instead so they never depend on it.
  static Future<ClockProbeSample> _defaultProbe(SupabaseClient client) async {
    final sentAt = DateTime.now().toUtc();
    final response = await client.rpc('bench_now');
    final receivedAt = DateTime.now().toUtc();
    final serverNow = DateTime.parse(response as String).toUtc();
    return ClockProbeSample(
      sentAt: sentAt,
      receivedAt: receivedAt,
      serverNow: serverNow,
    );
  }
}
