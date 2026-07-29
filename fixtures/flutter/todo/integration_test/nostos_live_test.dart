// The W5 acceptance proof: two Nostos instances (user-a / user-b), driven
// through NostosTodoRepository and a real nostos-server + real docker Postgres
// + real HS256 JWTs.
//
// ============================================================================
// FIXED (2026-07-12): nostos_flutter's watch() used to never deliver a row
// synced from a REAL PgReplicator-backed table if that row's transaction was
// never followed by another one — the single most common real shape for a
// todo app (one user, one action, then quiet). A second, independently
// reproduced bug meant a write made after a session was already connected
// (not part of the startup backlog) could sit in the outbox until the next
// reconnect. Both are fixed; see
// docs/adr/0016-client-sdk-and-wal-bloat-protection.md's addendum
// ("client apply flush bound + outbox re-flush trigger") for the full
// root-cause + fix writeup. Summary:
//   - `ApplyEngine::feed` (crates/nostos-core/src/apply.rs) buffered frames
//     sharing a `txn_id` and only flushed on a differing-`txn_id` follow-up
//     frame or the soft cap — no time-based fallback. Fixed by
//     `SyncClientConfig::flush_quiesce` (default 50ms): the connected loop
//     force-flushes a pending batch after a quiet gap, independent of
//     `idle_timeout` (which tears down the whole session — wrong hammer for
//     a long-lived client).
//   - `sdk/nostos_flutter/rust/src/api/nostos.rs` set `idle_timeout: None`
//     with no other backstop; it now sets a generous (120s) session-level
//     backstop, with `flush_quiesce` doing the real per-batch work.
//   - `SyncClient::run_once`'s outbox flush was a one-shot step run once
//     before the receive loop — a write enqueued mid-session had nothing to
//     wake it. Fixed by `SyncClient::write_notify`: `write()` now wakes the
//     connected loop to re-drain the outbox immediately.
//   - `Nostos`/`NostosHandle` had no way to tear down a subscription's
//     background tasks on demand (only on GC). Fixed: `Nostos.close()`
//     (`NostosHandle.close` in the generated bindings — named `close`, not
//     `dispose`, to avoid colliding with `RustOpaqueInterface`'s own
//     synchronous FFI-handle `dispose()`).
//
// New coverage proving the fix against a REAL `SyncClient` + REAL
// `PgReplicator` + real Postgres (this exact combination had never been
// tested anywhere in the repo before — `nostos-infra`'s e2e suite drives a
// raw `tokio-tungstenite` client, never `nostos-client::SyncClient`):
// `crates/nostos-client/tests/e2e_pg_sync.rs` (`NOSTOS_E2E_PG=1 cargo test -p
// nostos-client --features pg --test e2e_pg_sync`).
//
// Scenarios below that were `skip:`-marked pending this fix now run for
// real: (a2) and (d). (a1) and (b) were never skipped (they proved the write
// landed via a channel the bug didn't touch — direct Postgres polling / a
// raw WebSocket client) but were failing before this fix for the reasons
// above; they now pass through `watch()`/the repository's own sync path too.
// ============================================================================
//
// Prerequisite: `tool/nostos_live_up.sh` has already brought up docker
// Postgres + `nostos init` + a backgrounded `nostos dev` (see tool/nostos_env.sh
// for the fixed ws URL/port this file assumes). Run:
//
//   fixtures/flutter/todo/tool/nostos_live_up.sh
//   cd fixtures/flutter/todo && flutter test integration_test/nostos_live_test.dart -d macos
//   fixtures/flutter/todo/tool/nostos_live_down.sh
//
// Requires the same macOS App Sandbox opt-out as the SDK's own integration
// test (this file shells out to tool/mint_jwt.sh and `docker exec ... psql`
// — both subprocess spawns the sandbox blocks; see DebugProfile.entitlements).

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:todo/domain/todo_repository.dart';
import 'package:todo/infra/nostos_todo_repository.dart';

// Must match tool/nostos_env.sh.
const _wsUrl = 'ws://127.0.0.1:8810/sync';
const _healthUrl = 'http://127.0.0.1:8810/healthz';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  late String tokenA;
  late String tokenB;
  late Directory tmpDir;

  setUpAll(() async {
    try {
      final client = HttpClient();
      final req = await client
          .getUrl(Uri.parse(_healthUrl))
          .timeout(const Duration(seconds: 2));
      final res = await req.close();
      await res.drain<void>();
      client.close();
      expect(res.statusCode, 200);
    } catch (e) {
      fail(
        'nostos-server not reachable at $_healthUrl ($e). '
        'Run tool/nostos_live_up.sh first.',
      );
    }

    tokenA = await _mintToken('user-a');
    tokenB = await _mintToken('user-b');
    tmpDir = await Directory.systemTemp.createTemp('nostos_todo_live_it_');
  });

  tearDownAll(() => tmpDir.delete(recursive: true));

  testWidgets(
      '(a1) offline-safe create: write() returns fast and durable, and the '
      'row reaches Postgres (proven directly — NOT via watch(), see header)',
      (tester) async {
    final repoA = await NostosTodoRepository.connect(
      wsUrl: _wsUrl,
      token: tokenA,
      sqlitePath: '${tmpDir.path}/a-offline.sqlite',
    );
    final marker = 'offline-create-${DateTime.now().microsecondsSinceEpoch}';

    final sw = Stopwatch()..start();
    await repoA.add(marker);
    sw.stop();
    // The durable-outbox contract (ADR-0013): write() returns once the write
    // is captured on disk, NOT once the server acks it.
    expect(sw.elapsedMilliseconds, lessThan(1000),
        reason: 'write() must not block on the network round-trip');

    final landed = await _pollForTitle(marker, timeout: const Duration(seconds: 25));
    expect(landed, isTrue,
        reason: 'the write must still reach Postgres via the outbox flush + '
            'write-back path, independent of whether watch() reflects it');
  });

  testWidgets(
      '(a2) ...and it syncs back through watch() — the launch-blocking bug '
      '(see header) is fixed: a solitary write on an otherwise-idle table '
      'now reaches watch(), not just Postgres directly',
      (tester) async {
    final repoA = await NostosTodoRepository.connect(
      wsUrl: _wsUrl,
      token: tokenA,
      sqlitePath: '${tmpDir.path}/a-watch.sqlite',
    );
    final marker = 'watch-back-${DateTime.now().microsecondsSinceEpoch}';
    await repoA.add(marker);

    // Nothing else happens on this table/session — exactly the shape the
    // bug required: one write, then quiet. Before the fix this would hang
    // until the poll's own timeout; now `flush_quiesce` closes the batch
    // and `subscribe_changes`/`watch()`'s pump re-emits within ~50ms of it.
    final sawMarker = await repoA.watch().any((todos) {
      return todos.any((t) => t.title == marker);
    }).timeout(const Duration(seconds: 15), onTimeout: () => false);

    expect(sawMarker, isTrue,
        reason: 'watch() must reflect a solitary write on an otherwise-idle '
            'table within a bounded time, not buffer forever '
            '(ApplyEngine::feed / SyncClientConfig::flush_quiesce — see '
            'docs/adr/0016-client-sdk-and-wal-bloat-protection.md addendum)');
  });

  testWidgets(
      '(b) read isolation: a raw WS subscribe with NO filter still only '
      'shows the caller\'s own tenant (proven via a raw WebSocket client — '
      'nostos_flutter\'s watch() is blocked, see header; the claim under test '
      'is the SERVER\'s ADR-0011 enforcement, which a raw client observes '
      'identically to the SDK)', (tester) async {
    final markerA = 'iso-a-${DateTime.now().microsecondsSinceEpoch}';
    final markerB = 'iso-b-${DateTime.now().microsecondsSinceEpoch}';

    final wsA = await _RawSub.connect(tokenA, 'todos');
    final wsB = await _RawSub.connect(tokenB, 'todos');

    final repoA = await NostosTodoRepository.connect(
        wsUrl: _wsUrl,
        token: tokenA,
        sqlitePath: '${tmpDir.path}/a-iso.sqlite');
    final repoB = await NostosTodoRepository.connect(
        wsUrl: _wsUrl,
        token: tokenB,
        sqlitePath: '${tmpDir.path}/b-iso.sqlite');
    await repoA.add(markerA);
    await repoB.add(markerB);

    final results = await Future.wait([
      wsA.collectTitles(const Duration(seconds: 20)),
      wsB.collectTitles(const Duration(seconds: 20)),
    ]);
    final aTitles = results[0];
    final bTitles = results[1];
    await wsA.close();
    await wsB.close();

    expect(aTitles, contains(markerA));
    expect(aTitles, isNot(contains(markerB)),
        reason: 'user-a must never receive user-b\'s row (ADR-0011)');
    expect(bTitles, contains(markerB));
    expect(bTitles, isNot(contains(markerA)),
        reason: 'user-b must never receive user-a\'s row (ADR-0011)');
  });

  testWidgets(
      '(c) write isolation: user-a writing to user-b\'s todo id is rejected '
      '(proven at Postgres directly — the SDK has no client-visible '
      'rejection signal at all today, watch()-blocked or not; see '
      'NostosTodoRepository\'s class doc)', (tester) async {
    final bId = DateTime.now().microsecondsSinceEpoch.toString();
    const originalTitle = 'owned-by-b';

    // b creates a todo with a known id — a raw upsert via the public Nostos
    // API (NostosTodoRepository.add always generates its own id).
    await _rawUpsert(tokenB, bId, {'title': originalTitle, 'done': false},
        sqlitePath: '${tmpDir.path}/b-writeiso-raw.sqlite');
    final bWroteIt = await _pollForTitle(originalTitle,
        timeout: const Duration(seconds: 10));
    expect(bWroteIt, isTrue, reason: 'setup: b\'s own write must land first');

    // a attempts to upsert THE SAME id — a cross-tenant write attempt.
    await _rawUpsert(tokenA, bId, {'title': 'hijacked', 'done': false},
        sqlitePath: '${tmpDir.path}/a-writeiso-raw.sqlite');
    await Future<void>.delayed(const Duration(seconds: 2));

    final storedTitle = await _pgTitle(bId);
    expect(storedTitle, originalTitle,
        reason: 'a cross-tenant write must be rejected server-side '
            '(ADR-0018) — the row must be UNCHANGED in Postgres');
  });

  testWidgets(
      '(d) offline-first persistence: reopening the same local store shows '
      'the durable snapshot immediately', (tester) async {
    final dbPath = '${tmpDir.path}/persist.sqlite';
    final marker = 'persist-${DateTime.now().microsecondsSinceEpoch}';

    final nostos1 = await Nostos.connect(url: _wsUrl, token: tokenA, sqlitePath: dbPath);
    await nostos1.subscribe('todos');
    final id = DateTime.now().microsecondsSinceEpoch.toString();
    await nostos1
        .write('todos', op: 'upsert', pk: id, payload: {'title': marker, 'done': false});

    // Wait for the write to land in Postgres AND for nostos1's own echo to
    // apply locally (advancing its durable checkpoint) before reopening —
    // otherwise "shows it immediately on reopen" would trivially pass even
    // if persistence were broken (a fresh live delivery could cover for it).
    final landed =
        await _pollForTitle(marker, timeout: const Duration(seconds: 25));
    expect(landed, isTrue,
        reason: 'setup: the write must land before testing reopen persistence');
    await Future<void>.delayed(const Duration(seconds: 1));

    // Close nostos1 BEFORE opening a second connection to the SAME SQLite
    // file — nostos-client's SqliteStorage doesn't configure WAL/busy_timeout,
    // so two live connections to one file risk SQLITE_BUSY. This is also
    // this suite's first real exercise of the new close()/dispose surface
    // (see this file's header — `Nostos` previously had no way to tear down
    // a subscription's background tasks on demand).
    await nostos1.close();

    final nostos2 = await Nostos.connect(url: _wsUrl, token: tokenA, sqlitePath: dbPath);
    await nostos2.subscribe('todos');
    final firstSnapshot =
        await nostos2.watch('todos').first.timeout(const Duration(seconds: 3));
    await nostos2.close();

    expect(
      firstSnapshot.any((row) => row['title'] == marker),
      isTrue,
      reason: 'reopening the same local store must show the durable '
          'snapshot immediately — bounded to 3s, well under this file\'s '
          'network-poll timeouts, so a pass here can only be explained by '
          'the on-disk snapshot, not a fresh network round-trip (the '
          'server would not even re-deliver this row: resume_lsn is read '
          'from the durable checkpoint nostos1 already advanced past it)',
    );
  });

  // Scenario (e) mirrors (a2)'s "add via repo, assert via watch()" shape, but
  // then exercises repo.remove() (the data-layer delete added in the CRUD
  // migration) and asserts BOTH the local watch() reflects the deletion AND
  // the row is gone from Postgres (collapsed-apply delete-back path,
  // ADR-0013).
  testWidgets(
      '(e) delete: repo.remove() deletes locally (watch()) and the delete '
      'reaches Postgres server-side (collapsed-apply delete-back, ADR-0013)',
      (tester) async {
    final repoA = await NostosTodoRepository.connect(
      wsUrl: _wsUrl,
      token: tokenA,
      sqlitePath: '${tmpDir.path}/a-delete.sqlite',
    );
    final marker = 'delete-${DateTime.now().microsecondsSinceEpoch}';
    await repoA.add(marker);

    // Capture the id via watch() — repo.add() generates it internally, so the
    // caller learns it only by reading back.
    final id = await _idForTitle(repoA, marker);
    expect(id, isNotNull,
        reason: 'setup: the row must appear in watch() before we delete it');

    await repoA.remove(id!);

    // (1) Local: watch() reflects the deletion within a bounded time.
    final sawRemoval = await repoA.watch().any((todos) {
      return todos.every((t) => t.title != marker);
    }).timeout(const Duration(seconds: 15), onTimeout: () => false);
    expect(sawRemoval, isTrue,
        reason: 'watch() must reflect the deletion locally '
            '(collapsed-apply delete-back path, ADR-0013)');

    // (2) Server: the row is gone from Postgres.
    final stillInPg =
        await _pollForTitle(marker, timeout: const Duration(seconds: 10));
    expect(stillInPg, isFalse,
        reason: 'the row must be deleted from Postgres server-side');
  });

  // Scenario (f) exercises repo.update() (the partial-update path added in the
  // CRUD migration). Patches title AND done in a single collapsed write, then
  // asserts BOTH the local watch() reflects the new field values AND the
  // patched title reaches Postgres (per-field LWW, ADR-0014).
  testWidgets(
      '(f) patch/update: repo.update() patches title+done in one write, '
      'watch() reflects the new state, and the patched title reaches Postgres',
      (tester) async {
    final repoA = await NostosTodoRepository.connect(
      wsUrl: _wsUrl,
      token: tokenA,
      sqlitePath: '${tmpDir.path}/a-patch.sqlite',
    );
    final marker = 'patch-${DateTime.now().microsecondsSinceEpoch}';
    await repoA.add(marker);

    final id = await _idForTitle(repoA, marker);
    expect(id, isNotNull,
        reason: 'setup: the row must appear in watch() before we patch it');

    final patchedTitle = '$marker-patched';
    await repoA.update(id!, title: patchedTitle, done: true);

    // Local: watch() reflects the patched title AND the done flag — on the
    // SAME row (matched by id, not title, so the assertion stays sound even
    // if other rows with the marker title still exist).
    final sawPatch = await repoA.watch().any((todos) {
      for (final t in todos) {
        if (t.id == id) return t.title == patchedTitle && t.done;
      }
      return false;
    }).timeout(const Duration(seconds: 15), onTimeout: () => false);
    expect(sawPatch, isTrue,
        reason: 'watch() must reflect the patched title AND done flag on the '
            'matching row (per-field LWW, ADR-0014)');

    // Server: the row in Postgres carries the new title.
    final landed = await _pollForTitle(patchedTitle,
        timeout: const Duration(seconds: 10));
    expect(landed, isTrue,
        reason: 'the patched title must reach Postgres server-side');
  });
}

/// Waits for a row titled [title] to appear in [repo]'s watch() stream, then
/// returns its id. Returns null on timeout. repo.add() generates the id
/// internally (server-side), so callers in scenarios (e) and (f) need to read
/// it back via watch() before they can target a specific row for delete/patch.
Future<String?> _idForTitle(
  NostosTodoRepository repo,
  String title, {
  Duration timeout = const Duration(seconds: 15),
}) {
  String? captured;
  late StreamSubscription<List<Todo>> sub;
  final completer = Completer<String?>();
  sub = repo.watch().listen((snapshot) {
    for (final t in snapshot) {
      if (t.title == title) {
        captured = t.id;
        break;
      }
    }
    if (captured != null && !completer.isCompleted) {
      completer.complete(captured);
      sub.cancel();
    }
  });
  return completer.future.timeout(timeout, onTimeout: () {
    sub.cancel();
    return null;
  });
}

Future<String> _mintToken(String sub) async {
  final result = await Process.run(
    '${Directory.current.path}/tool/mint_jwt.sh',
    [sub],
  );
  if (result.exitCode != 0) {
    fail('mint_jwt.sh failed: ${result.stderr}');
  }
  return (result.stdout as String).trim();
}

/// A raw upsert against an explicit pk — [NostosTodoRepository.add] always
/// generates its own id, so scenario (c) (which needs to target a SPECIFIC,
/// already-known row) goes through the public [Nostos] API directly instead,
/// exactly as a caller who isn't using the repository wrapper would. write()
/// itself is proven NOT blocked by the watch() bug (see (a1)) — only the
/// read-back through watch() is.
Future<void> _rawUpsert(
  String token,
  String pk,
  Map<String, dynamic> payload, {
  required String sqlitePath,
}) async {
  final nostos = await Nostos.connect(
      url: _wsUrl, token: token, sqlitePath: sqlitePath);
  await nostos.subscribe('todos');
  await nostos.write('todos', op: 'upsert', pk: pk, payload: payload);
}

/// Poll Postgres directly until a row with the given title exists (or
/// [timeout] elapses). Used everywhere this file needs to know "did the
/// write actually land" WITHOUT going through nostos_flutter's blocked
/// watch() — see this file's header.
Future<bool> _pollForTitle(String title, {required Duration timeout}) async {
  final deadline = DateTime.now().add(timeout);
  while (DateTime.now().isBefore(deadline)) {
    final t = await _pgTitleByTitleQuery(title);
    if (t) return true;
    await Future<void>.delayed(const Duration(milliseconds: 300));
  }
  return false;
}

Future<bool> _pgTitleByTitleQuery(String title) async {
  final result = await Process.run('docker', [
    'exec', 'nostos-postgres', 'psql', '-U', 'cairn', '-d', 'cairn', '-tAc',
    "SELECT 1 FROM todos WHERE title='$title' LIMIT 1;",
  ]);
  return (result.stdout as String).trim() == '1';
}

Future<String?> _pgTitle(String id) async {
  final result = await Process.run('docker', [
    'exec',
    'nostos-postgres',
    'psql',
    '-U',
    'cairn',
    '-d',
    'cairn',
    '-tAc',
    "SELECT title FROM todos WHERE id='$id'",
  ]);
  if (result.exitCode != 0) {
    fail('psql query failed: ${result.stderr}');
  }
  final out = (result.stdout as String).trim();
  return out.isEmpty ? null : out;
}

/// A raw WebSocket subscriber — bypasses nostos_flutter's `ApplyEngine`
/// batching entirely (see this file's header), so it observes the SERVER's
/// tenant-scoped fan-out (ADR-0011) directly and reliably.
class _RawSub {
  _RawSub._(this._ws);
  final WebSocket _ws;

  static Future<_RawSub> connect(String token, String table) async {
    final ws = await WebSocket.connect('ws://127.0.0.1:8810/sync?token=$token');
    ws.add(jsonEncode({'type': 'subscribe', 'table': table, 'filters': []}));
    return _RawSub._(ws);
  }

  /// Collects every decoded row's `title` seen within [duration].
  Future<Set<String>> collectTitles(Duration duration) async {
    final titles = <String>{};
    final sub = _ws.listen((data) {
      final bytes = data is String ? utf8.encode(data) : data as List<int>;
      try {
        final frame = jsonDecode(utf8.decode(bytes)) as Map<String, dynamic>;
        final hex = frame['payload'] as String?;
        if (hex == null) return;
        final payloadBytes = <int>[
          for (var i = 0; i < hex.length; i += 2)
            int.parse(hex.substring(i, i + 2), radix: 16),
        ];
        final row = jsonDecode(utf8.decode(payloadBytes)) as Map<String, dynamic>;
        final title = row['title'] as String?;
        if (title != null) titles.add(title);
      } catch (_) {
        // Non-row control frames (e.g. write_result) — ignore.
      }
    });
    await Future<void>.delayed(duration);
    await sub.cancel();
    return titles;
  }

  Future<void> close() => _ws.close();
}
