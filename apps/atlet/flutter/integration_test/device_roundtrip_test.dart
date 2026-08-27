// The no-credentials device round-trip: the A5/B5 sync substance without
// any operator-owned secrets. push_smoke_test.dart proves the full real-rail
// doorbell (FCM + Supabase auth) but therefore self-skips on a box without
// the Firebase service account / a live Supabase project. This companion
// proves the part that needs NO credentials but DOES need real hardware:
// a real device/emulator builds, connects over the network to a genuine
// nostos-server (pg replicator), first-syncs, goes OFFLINE (pauseSync),
// receives a server-side committed row while offline, resumes (resumeSync),
// and observes the row arrive — the durable-LSN delta apply, not a resync.
//
// Drive it with tool/device_roundtrip.sh (mirrors push_smoke.sh's server
// leg: docker PG + nostos-server with NOSTOS_SYNC_AUTH=none, NO FCM, NO
// Supabase). The harness inserts the triggering row AFTER seeing
// DEVICE_ROUNDTRIP_READY, while the app is paused.
//
//     apps/atlet/flutter/tool/device_roundtrip.sh                 # android
//     PUSH_SMOKE_DEVICE=ios PUSH_SMOKE_DEVICE_ID=<id> \
//       NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:8081/sync \
//       apps/atlet/flutter/tool/device_roundtrip.sh               # iOS device

import 'dart:async';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:path_provider/path_provider.dart';

const _syncUrl = String.fromEnvironment('NOSTOS_SYNC_URL');
const _rowId = String.fromEnvironment('ROUNDTRIP_ROW_ID');
// Window the harness has to insert the row while we hold offline. Generous:
// the harness reacts to DEVICE_ROUNDTRIP_READY in a poll loop.
const _insertWindowSecs = int.fromEnvironment(
  'ROUNDTRIP_INSERT_SECS',
  defaultValue: 12,
);

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('device offline→online round-trip on a live nostos-server', (
    tester,
  ) async {
    if (_syncUrl.isEmpty || _rowId.isEmpty) {
      throw StateError(
        'NOSTOS_SYNC_URL / ROUNDTRIP_ROW_ID dart-defines required — '
        'run via apps/atlet/flutter/tool/device_roundtrip.sh',
      );
    }

    // Visible-liveness, same convention as the push smoke.
    await tester.pumpWidget(
      const Directionality(
        textDirection: TextDirection.ltr,
        child: ColoredBox(
          color: Color(0xFF1C1B1F),
          child: Center(
            child: Text('atlet round-trip — paused, waiting for the insert…'),
          ),
        ),
      ),
    );

    final dir = await getApplicationDocumentsDirectory();
    // The seed row (inserted by the harness BEFORE the app ran) must arrive
    // in the snapshot — this is what proves first-sync really delivered
    // rows. Without it, waitForFirstSync can complete even when the server
    // is rejecting subscribes in a reconnect loop (observed 2026-08-27 with
    // claim-gated hand-mode rules + auth=none) and the test would lie.
    const seedId = String.fromEnvironment('ROUNDTRIP_SEED_ID');
    if (seedId.isEmpty) {
      throw StateError('ROUNDTRIP_SEED_ID dart-define required (harness seeds a row pre-launch)');
    }
    // Scratch store — disposable, never the real app database.
    final db = await NostosDatabase.connect(
      url: _syncUrl,
      sqlitePath: '${dir.path}/nostos_roundtrip.sqlite',
    );
    await db.subscribeTables(const [NostosTableSub(name: 'sessions')]);
    await db.waitForFirstSync();
    // watch() is legal only after subscribe() (single-table guard), and it
    // replays the latest snapshot — so the seed arrives on first emission.
    final sawSeed = Completer<void>();
    late final StreamSubscription sub0;
    sub0 = db.watch('SELECT * FROM sessions ORDER BY _pk').listen((rows) {
      for (final row in rows) {
        if (row['id'] == seedId && !sawSeed.isCompleted) {
          debugPrint('DEVICE_ROUNDTRIP_SEED_SEEN snapshot delivered');
          sawSeed.complete();
        }
      }
    });
    await sawSeed.future.timeout(const Duration(seconds: 60));
    await sub0.cancel();
    debugPrint('DEVICE_ROUNDTRIP_SYNCED first-sync complete (seed row observed)');

    // OFFLINE: the harness's insert must land while nobody is listening —
    // the delta then applies from the durable LSN checkpoint on resume.
    await db.pauseSync();
    debugPrint('DEVICE_ROUNDTRIP_READY paused, insert window open');

    final sawRow = Completer<void>();
    final sub = db
        .watch('SELECT * FROM sessions ORDER BY _pk')
        .listen((rows) {
          for (final row in rows) {
            if (row['id'] == _rowId && !sawRow.isCompleted) {
              debugPrint('DEVICE_ROUNDTRIP_PASS row=$_rowId');
              sawRow.complete();
            }
          }
        });

    try {
      // Hold offline through the insert window, then resume and wait for the
      // delta. The row CANNOT arrive before resumeSync — if it does, the
      // pause was not a real offline (that would be a finding, not a pass).
      await Future<void>.delayed(Duration(seconds: _insertWindowSecs));
      db.resumeSync();
      debugPrint('DEVICE_ROUNDTRIP_RESUMED');
      await sawRow.future.timeout(const Duration(seconds: 90));
      debugPrint('DEVICE_ROUNDTRIP_DONE');
    } finally {
      await sub.cancel();
      try {
        await db.close().timeout(const Duration(seconds: 15));
      } on TimeoutException {
        debugPrint('DEVICE_ROUNDTRIP_CLOSE_TIMEOUT store left to process exit');
      }
    }
  });
}
