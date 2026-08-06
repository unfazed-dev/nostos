// Tests for lib/bench/upload.dart (task-15). No live Supabase/network in
// this environment (see task-14-report.md) — [RunsUploader] is a plain
// injectable closure, so `uploadStoredRuns` is exercised against a fake
// that records what it was called with, exactly like harness_test.dart's
// fake `insertRemoteRow`.

import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/bench/runner.dart';
import 'package:atlet/bench/store.dart';
import 'package:atlet/bench/upload.dart';

RunRecord _fixture({String engine = 'cairn', String runType = 'cold_sync'}) =>
    RunRecord(
      sdk: 'flutter',
      engine: engine,
      profile: 'local',
      runType: runType,
      specVersion: '1.0',
      seedSize: 100,
      appVersion: '0.1.0',
      device: {'model': 'test'},
      metrics: {'cold_sync_ms': 42},
      startedAt: DateTime.utc(2026, 8, 1),
    );

void main() {
  late Directory tempDir;

  setUp(() async {
    tempDir = await Directory.systemTemp.createTemp('atlet_upload_test_');
  });

  tearDown(() async {
    if (await tempDir.exists()) await tempDir.delete(recursive: true);
  });

  test('uploadStoredRuns returns 0 and never calls uploadRuns on an empty store', () async {
    final store = BenchStore(directory: tempDir);
    var called = false;
    final count = await uploadStoredRuns(store, (rows) async {
      called = true;
    });
    expect(count, 0);
    expect(called, isFalse);
  });

  test('uploadStoredRuns posts every stored record shaped via toJson', () async {
    final store = BenchStore(directory: tempDir);
    await store.append(_fixture(runType: 'cold_sync'));
    await store.append(_fixture(runType: 'propagation'));

    List<Map<String, dynamic>>? captured;
    final count = await uploadStoredRuns(store, (rows) async {
      captured = rows;
    });

    expect(count, 2);
    expect(captured, isNotNull);
    expect(captured!.length, 2);
    expect(captured![0]['run_type'], 'cold_sync');
    expect(captured![1]['run_type'], 'propagation');
    // Every uploaded row carries the internal-eval label (decision #10) —
    // RunRecord.toJson stamps it into metrics, upload.dart must not strip it.
    expect(captured![0]['metrics']['label'], RunRecord.evaluationLabel);
  });

  test('uploadStoredRuns propagates a failing uploader instead of swallowing it', () async {
    final store = BenchStore(directory: tempDir);
    await store.append(_fixture());

    Future<void> failingUpload(List<Map<String, dynamic>> rows) async {
      throw StateError('network down');
    }

    expect(
      () => uploadStoredRuns(store, failingUpload),
      throwsA(isA<StateError>()),
    );
  });
}
