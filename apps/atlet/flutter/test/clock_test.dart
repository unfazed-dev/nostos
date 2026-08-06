import 'package:atlet/bench/clock.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('BenchClock.estimateOffset', () {
    test('server ahead by 120ms, RTT 40ms -> offset ~120ms', () async {
      // Fake probe: fixed 40ms RTT, server clock skewed +120ms ahead of the
      // client's mid-RTT instant. offset = serverNow - (sentAt + rtt/2).
      Future<ClockProbeSample> fakeProbe() async {
        final sentAt = DateTime.utc(2026, 1, 1, 0, 0, 0);
        final rtt = const Duration(milliseconds: 40);
        final receivedAt = sentAt.add(rtt);
        final serverNow = sentAt.add(
          const Duration(milliseconds: 20 + 120), // mid-rtt + 120ms skew
        );
        return ClockProbeSample(
          sentAt: sentAt,
          receivedAt: receivedAt,
          serverNow: serverNow,
        );
      }

      final offset = await BenchClock.estimateOffset(probe: fakeProbe);

      expect(offset.inMilliseconds, closeTo(120, 5));
    });

    test('median is robust to a single outlier sample', () async {
      var call = 0;
      Future<ClockProbeSample> fakeProbe() async {
        call += 1;
        final sentAt = DateTime.utc(2026, 1, 1, 0, 0, 0);
        final rtt = const Duration(milliseconds: 40);
        final receivedAt = sentAt.add(rtt);
        // 4 samples agree on ~100ms skew, 1 sample is a wild 5000ms outlier.
        final skewMs = call == 3 ? 5000 : 100;
        final serverNow = sentAt.add(Duration(milliseconds: 20 + skewMs));
        return ClockProbeSample(
          sentAt: sentAt,
          receivedAt: receivedAt,
          serverNow: serverNow,
        );
      }

      final offset = await BenchClock.estimateOffset(probe: fakeProbe);

      expect(offset.inMilliseconds, closeTo(100, 5));
      expect(call, 5); // exactly 5 probes per spec/metrics.md
    });

    test('zero skew, symmetric RTT -> offset ~0', () async {
      Future<ClockProbeSample> fakeProbe() async {
        final sentAt = DateTime.utc(2026, 1, 1, 0, 0, 0);
        final rtt = const Duration(milliseconds: 10);
        final receivedAt = sentAt.add(rtt);
        final serverNow = sentAt.add(const Duration(milliseconds: 5));
        return ClockProbeSample(
          sentAt: sentAt,
          receivedAt: receivedAt,
          serverNow: serverNow,
        );
      }

      final offset = await BenchClock.estimateOffset(probe: fakeProbe);

      expect(offset.inMilliseconds, closeTo(0, 2));
    });
  });
}
