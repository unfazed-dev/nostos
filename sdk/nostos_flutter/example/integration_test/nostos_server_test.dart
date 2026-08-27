// The W4 acceptance integration test: connect → subscribe → server fans out
// events → watch() emits, against a REAL `nostos-server` binary (not an
// in-process test harness) — proving the packaging path (native-assets +
// prebuilt-manifest hook) AND the sync loop work inside a genuine app bundle
// (see docs/plans/w4-packaging-fallback.md's W0a spike, which proved the
// packaging mechanism in isolation; this proves the real SDK on top of it).
//
// Run from `sdk/nostos_flutter/example/`:
//   flutter test integration_test/nostos_server_test.dart -d macos
//
// Spins up `cargo run -p nostos-server` itself (zero-setup default:
// NOSTOS_REPLICATOR=fake, NOSTOS_SYNC_AUTH=none — see crates/nostos-server's
// README/quickstart). The fake replicator emits u64::MAX synthetic `tasks`
// events continuously from server boot (crates/nostos-server/src/main.rs),
// independent of when a client subscribes, so a late-connecting client still
// sees a live stream — confirmed by reading the source before relying on it.
//
// ponytail: the fake replicator's payload is deterministic filler bytes, NOT
// JSON (only PgReplicator's tuple_to_json_payload produces JSON — see
// rust/src/api/nostos.rs's `row_to_json_object` doc). So this test asserts
// rows arrive and decode to a Map (via the `_raw` hex fallback), not that
// they carry meaningful columns — proving the pipeline, not the payload
// shape. A real Postgres-backed deployment gets real JSON columns.

import 'dart:convert';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
// `NostosTable`/`NostosColumn` are intentionally NOT re-exported from the package barrel
// (they would shadow material's `NostosTable`/`NostosColumn` widgets — see
// lib/nostos_flutter.dart). Reach them via the src/ import when binding by
// name, as here.
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

const _bind = '127.0.0.1:8801';
const _healthUrl = 'http://$_bind/healthz';
const _syncUrl = 'ws://$_bind/sync';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  late Process server;

  setUpAll(() async {
    final repoRoot =
        Platform.environment['NOSTOS_REPO_ROOT'] ??
        '${Directory.current.path}/../../..';

    // 1) Compile FIRST, under its own generous budget. The /healthz window in
    //    step 2 exists to measure server BOOT (~1s warm) — a cold compile of
    //    the server graph (fingerprint walk + build) can take tens of minutes
    //    on slow/external storage (observed 2026-08-27: cargo spent >50min in
    //    fingerprint::calculate on an external SSD before any rustc), which
    //    made the old 2-minute run-window fail for reasons that had nothing
    //    to do with the server. Compiling up front keeps the boot window
    //    meaningful: if the healthz poll fails below, it is a genuine server
    //    failure. Budget default 15min; raise via NOSTOS_IT_COMPILE_BUDGET_MIN
    //    on cold or slow machines.
    final compileBudget = Duration(
      minutes:
          int.tryParse(
            Platform.environment['NOSTOS_IT_COMPILE_BUDGET_MIN'] ?? '',
          ) ??
          15,
    );
    // Rule-file isolation: the server resolves nostos_rules.toml relative
    // to ITS cwd (repo root), and the operator's atlet dev rules there are
    // hand-mode with claim-gated scopes — the 'tasks' table this test uses
    // is not listed, so every subscribe would be rejected and watch() would
    // starve (observed 2026-08-27 as a 15s TimeoutException). Point the
    // server at an explicit all-mode rules file instead — same semantics as
    // the documented zero-config default (no file found → all).
    final rulesDir = await Directory.systemTemp.createTemp('nostos_it_rules_');
    final rulesPath = '${rulesDir.path}/nostos_rules_all.toml';
    File(rulesPath).writeAsStringSync('version = 1\nsync_mode = "all"\n');
    final compile = await Process.start('cargo', [
      'build',
      '-p',
      'nostos-server',
      '--quiet',
    ], workingDirectory: repoRoot);
    // Drain stderr so a full pipe can never deadlock the build; keep it for
    // the failure message.
    final compileErr = <String>[];
    compile.stderr.transform(utf8.decoder).listen(compileErr.add);
    final compileExit = await compile.exitCode.timeout(
      compileBudget,
      onTimeout: () {
        compile.kill();
        return -1;
      },
    );
    if (compileExit != 0) {
      final budgetHint = compileExit == -1
          ? 'Budget exceeded — raise NOSTOS_IT_COMPILE_BUDGET_MIN. '
          : '';
      fail(
        'cargo build -p nostos-server did not succeed (exit $compileExit) '
        'within ${compileBudget.inMinutes}min. '
        '$budgetHint'
        'stderr: ${compileErr.join()}',
      );
    }

    // 2) Boot the (now warm) server and poll /healthz — this window measures
    //    boot only.
    server = await Process.start(
      'cargo',
      ['run', '-p', 'nostos-server', '--quiet'],
      workingDirectory: repoRoot,
      environment: {'NOSTOS_BIND': _bind, 'NOSTOS_RULES_FILE': rulesPath},
    );
    // Surface server-side failures in the test log instead of a silent hang.
    server.stderr
        .transform(utf8.decoder)
        .listen((line) => stderr.writeln('[nostos-server] $line'));

    final deadline = DateTime.now().add(const Duration(minutes: 2));
    while (DateTime.now().isBefore(deadline)) {
      try {
        final client = HttpClient();
        final req = await client.getUrl(Uri.parse(_healthUrl));
        final res = await req.close();
        await res.drain<void>();
        client.close();
        if (res.statusCode == 200) return;
      } catch (_) {
        // Not up yet (port not bound) — keep polling.
      }
      await Future<void>.delayed(const Duration(milliseconds: 500));
    }
    fail(
      'nostos-server did not become healthy at $_healthUrl within 2 minutes '
      'of a warm binary — check stderr output above for a boot failure',
    );
  });

  tearDownAll(() {
    server.kill(ProcessSignal.sigterm);
  });

  testWidgets('connect -> subscribe -> server fans out -> watch() emits', (
    tester,
  ) async {
    final dbDir = await Directory.systemTemp.createTemp('nostos_flutter_it_');
    addTearDown(() => dbDir.delete(recursive: true));

    // The fake replicator has NO SchemaSource → GET /schema returns 404, so
    // NostosDatabase.connect's auto-fetch would throw. Pass an explicit
    // schema so applySchema runs locally (creating the WS2 `tasks` view)
    // without the HTTP round-trip. Columns carry no affinity here (hand-built,
    // no /schema fetch) — affinity is nullable for exactly this case (WS6).
    final schema = NostosSchema(
      tables: [
        NostosTable(
          name: 'tasks',
          primaryKey: const ['id'],
          columns: const [
            NostosColumn(name: 'title'),
            NostosColumn(name: 'completed'),
          ],
        ),
      ],
    );

    final db = await NostosDatabase.connect(
      url: _syncUrl,
      schema: schema,
      sqlitePath: '${dbDir.path}/it.sqlite',
    );
    // addTearDown runs LIFO: this close (registered last) runs BEFORE the
    // dbDir.delete (registered first), so the SQLite handle releases the file
    // before we try to delete its directory (else errno 66 ENOTEMPTY).
    addTearDown(db.close);

    final connected = db.connectionState
        .firstWhere((s) => s == NostosConnectionState.connected)
        .timeout(const Duration(seconds: 15));

    await db.subscribe('tasks');

    await expectLater(connected, completes);

    // Read just `_pk` from the WS2 `tasks` view. The fake replicator's payload
    // is OPAQUE BYTES, not JSON (crates/nostos-server/src/main.rs:279-281 — only
    // PgReplicator emits a JSON object), so SQLite's json_extract on it ERRORS
    // with "malformed JSON" — it does NOT return NULL (that was the prior
    // assumption; running this test live falsified it). `SELECT * FROM tasks`
    // evaluates the json_extract'd title/completed columns and errors; `SELECT
    // _pk` projects only the row key (SQLite prunes the unused json_extract
    // columns) — the pipeline proof this test exists for. Typed-column
    // rendering against valid-JSON pg payloads is proven by the demo +
    // probe-runner; the view's `_pk` projection by the nostos-client unit test.
    final rows = await db
        .watch('SELECT _pk FROM tasks')
        .firstWhere((rows) => rows.isNotEmpty)
        .timeout(const Duration(seconds: 15));

    expect(rows, isNotEmpty);
    expect(
      rows.first,
      contains('_pk'),
      reason: 'every decoded row carries its primary key (pk AS _pk)',
    );

    // WS6 typed-record path: watchMapped decodes rows into typed records via a
    // user fromRow, end-to-end against the real engine + FFI. `_pk` is the one
    // value the fake replicator populates (the row key; payload columns are
    // NULL), so map on it — the meaningful typed-field cast (title/completed)
    // is covered by the pure-Dart unit test with canned real-shaped JSON.
    final typed = await db
        .watchMapped<String>(
          'SELECT _pk FROM tasks',
          (row) => row['_pk'].toString(),
        )
        .firstWhere((keys) => keys.isNotEmpty)
        .timeout(const Duration(seconds: 15));
    expect(typed, isNotEmpty);
  });
}
