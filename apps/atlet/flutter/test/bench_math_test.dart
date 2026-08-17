import 'dart:io';

import 'package:atlet/bench/runner.dart';
import 'package:atlet/bench/store.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('median', () {
    test('odd length', () {
      expect(median([3, 1, 2]), 2.0);
    });

    test('even length averages middle pair', () {
      expect(median([1, 2, 3, 4]), 2.5);
    });

    test('single value', () {
      expect(median([42]), 42.0);
    });

    test('unsorted input is not mutated by the algorithm caller', () {
      final input = [5, 3, 1, 4, 2];
      final result = median(input);
      expect(result, 3.0);
      expect(input, [5, 3, 1, 4, 2]); // caller's list must survive
    });

    test('empty list throws', () {
      expect(() => median(<num>[]), throwsArgumentError);
    });
  });

  group('p95 (linear interpolation, PERCENTILE.INC-compatible)', () {
    test('1..100 known vector', () {
      final values = List<num>.generate(100, (i) => i + 1);
      expect(p95(values), closeTo(95.05, 0.01));
    });

    test('single value', () {
      expect(p95([7]), 7.0);
    });

    test('empty list throws', () {
      expect(() => p95(<num>[]), throwsArgumentError);
    });
  });

  group('percentile', () {
    test('p50 matches median on even-length vector', () {
      final values = [1, 2, 3, 4];
      expect(percentile(values, 50), median(values));
    });

    test('p0 is the minimum, p100 is the maximum', () {
      final values = [5, 1, 9, 3];
      expect(percentile(values, 0), 1.0);
      expect(percentile(values, 100), 9.0);
    });

    test('rejects out-of-range percentile', () {
      expect(() => percentile([1, 2, 3], -1), throwsArgumentError);
      expect(() => percentile([1, 2, 3], 101), throwsArgumentError);
    });
  });

  group('RunRecord JSONL round-trip', () {
    test('toJson/fromJson preserves fields', () {
      final record = RunRecord(
        sdk: 'flutter',
        engine: 'cairn',
        profile: 'local',
        runType: 'write_ack',
        specVersion: 'v0',
        seedSize: 100,
        appVersion: '1.0.0+1',
        device: {'model': 'iPhone15,2', 'os': 'iOS 18.1'},
        metrics: {
          'write_ack_ms_median': 42.0,
          'write_ack_ms_p95': 88.5,
          'n': 25,
        },
        startedAt: DateTime.utc(2026, 8, 6, 12, 30),
      );

      final decoded = RunRecord.fromJson(record.toJson());

      expect(decoded.sdk, record.sdk);
      expect(decoded.engine, record.engine);
      expect(decoded.profile, record.profile);
      expect(decoded.runType, record.runType);
      expect(decoded.specVersion, record.specVersion);
      expect(decoded.seedSize, record.seedSize);
      expect(decoded.appVersion, record.appVersion);
      expect(decoded.device['model'], 'iPhone15,2');
      expect(decoded.metrics['write_ack_ms_median'], 42.0);
      expect(decoded.metrics['n'], 25);
      expect(decoded.startedAt, record.startedAt);
    });

    test('toJson always stamps the evaluation-only label', () {
      final record = RunRecord(
        sdk: 'flutter',
        engine: 'powersync',
        profile: 'cloud',
        runType: 'cold_sync',
        specVersion: 'v0',
        seedSize: 50,
        appVersion: '1.0.0+1',
        device: {'model': 'Pixel 8', 'os': 'Android 15'},
        metrics: {'cold_sync_ms': 1200},
        startedAt: DateTime.utc(2026, 8, 6),
      );

      expect(
        record.toJson()['metrics']['label'],
        RunRecord.evaluationLabel,
      );
    });
  });

  group('BenchStore JSONL round-trip', () {
    late Directory tempDir;
    late BenchStore store;

    setUp(() {
      tempDir = Directory.systemTemp.createTempSync('atlet_bench_store_');
      store = BenchStore(directory: tempDir);
    });

    tearDown(() {
      tempDir.deleteSync(recursive: true);
    });

    test('append then readAll returns the same records in order', () async {
      final a = RunRecord(
        sdk: 'flutter',
        engine: 'cairn',
        profile: 'local',
        runType: 'cold_sync',
        specVersion: 'v0',
        seedSize: 10,
        appVersion: '1.0.0+1',
        device: {'model': 'a', 'os': 'a-os'},
        metrics: {'cold_sync_ms': 500},
        startedAt: DateTime.utc(2026, 1, 1),
      );
      final b = RunRecord(
        sdk: 'flutter',
        engine: 'cairn',
        profile: 'local',
        runType: 'propagation',
        specVersion: 'v0',
        seedSize: 10,
        appVersion: '1.0.0+1',
        device: {'model': 'a', 'os': 'a-os'},
        metrics: {'propagation_ms_median': 33.0, 'propagation_ms_p95': 61.0},
        startedAt: DateTime.utc(2026, 1, 1, 0, 1),
      );

      await store.append(a);
      await store.append(b);

      final records = await store.readAll();
      expect(records, hasLength(2));
      expect(records[0].runType, 'cold_sync');
      expect(records[1].runType, 'propagation');
      expect(records[1].metrics['propagation_ms_median'], 33.0);
    });

    test('each record is one line (JSONL, not a JSON array)', () async {
      await store.append(
        RunRecord(
          sdk: 'flutter',
          engine: 'cairn',
          profile: 'local',
          runType: 'queue_drain',
          specVersion: 'v0',
          seedSize: 25,
          appVersion: '1.0.0+1',
          device: {'model': 'a', 'os': 'a-os'},
          metrics: {'queue_drain_ms': 900},
          startedAt: DateTime.utc(2026, 1, 1),
        ),
      );
      await store.append(
        RunRecord(
          sdk: 'flutter',
          engine: 'cairn',
          profile: 'local',
          runType: 'queue_drain',
          specVersion: 'v0',
          seedSize: 25,
          appVersion: '1.0.0+1',
          device: {'model': 'a', 'os': 'a-os'},
          metrics: {'queue_drain_ms': 950},
          startedAt: DateTime.utc(2026, 1, 1, 0, 1),
        ),
      );

      final lines = await store.file!.readAsLines();
      expect(lines, hasLength(2));
    });
  });
}
