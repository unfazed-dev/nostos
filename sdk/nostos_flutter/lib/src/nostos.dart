import 'dart:async';
import 'dart:convert';

import 'package:meta/meta.dart';
import 'package:path_provider/path_provider.dart';

import 'engine.dart';
import 'rust/frb_generated.dart';

export 'engine.dart' show NostosConnectionState;

/// A live Nostos sync connection.
///
/// No connector class, no client-side schema artifact: rows are plain
/// `Map<String, dynamic>` decoded from the same JSON payload the server
/// delivers (or `PgReplicator` produces from Postgres). Rust owns SQLite and
/// the sync loop; this class is a thin, reactive Dart wrapper.
///
/// v1 supports **one active subscription per `Nostos` instance** — this
/// mirrors `nostos-client`'s `SyncClient`, which binds one table at
/// construction (see `rust/src/api/nostos.rs` module docs). Calling
/// [subscribe] again replaces the previous subscription (its background
/// connection is torn down). Multiple independent table subscriptions from
/// one app means multiple `Nostos.connect()` instances for now — a ponytail
/// for a future multi-table `nostos-client` session.
class Nostos {
  Nostos._(this._engine);

  /// Test-only constructor: inject a fake [NostosEngine] to exercise this
  /// class's wiring (subscribe/watch/write, table-mismatch errors, JSON
  /// decode fallback) without the native library. See `test/nostos_test.dart`.
  @visibleForTesting
  Nostos.withEngine(NostosEngine engine) : this._(engine);

  final NostosEngine _engine;

  static bool _rustInitialized = false;

  /// Open a connection to a `nostos-server` `/sync` endpoint. Does not touch
  /// the network yet — [subscribe] starts the actual session.
  ///
  /// [url] is the WebSocket URL, e.g. `ws://localhost:8800/sync` (see
  /// `nostos dev`'s printed URL). [token] is a bearer token sent as `?token=`
  /// on the WS handshake (matches whatever `NOSTOS_SYNC_AUTH` mode the server
  /// runs — `none` ignores it, `supabase-jwt` verifies it).
  ///
  /// [sqlitePath] overrides where the durable client store lives; omit it to
  /// use a per-`url` default under the platform's application-support
  /// directory (via `path_provider`) — zero manual steps for the common case.
  static Future<Nostos> connect({
    required String url,
    String? token,
    String? sqlitePath,
  }) async {
    if (!_rustInitialized) {
      await RustLib.init();
      _rustInitialized = true;
    }
    final path = sqlitePath ?? await _defaultSqlitePath(url);
    return Nostos._(RustNostosEngine.connect(url: url, token: token, dbPath: path));
  }

  String? _subscribedTable;
  Stream<List<Map<String, dynamic>>>? _rowsStream;
  final StreamController<NostosConnectionState> _stateController =
      StreamController<NostosConnectionState>.broadcast();

  /// Connection-state transitions for the current (or most recently started)
  /// subscription. Empty until [subscribe] has been called at least once.
  Stream<NostosConnectionState> get connectionState => _stateController.stream;

  /// Subscribe to [table], optionally filtered by [where] — a safe-SQL subset
  /// predicate (ADR-0012), e.g. `"status = 'open' AND priority >= 3"`. The
  /// server compiles and ANDs it into the session; a parse failure closes the
  /// socket (surfaces as a [connectionState] flip, not an exception here —
  /// see `SyncClientConfig.where_sql`'s doc in nostos-client).
  ///
  /// Replaces any previous subscription on this instance (v1: one table per
  /// `Nostos` — see the class doc).
  Future<void> subscribe(String table, {String? where}) async {
    final streams = await _engine.subscribe(table: table, whereSql: where);
    _subscribedTable = table;
    _rowsStream = streams.rows.map(_decodeRows).asBroadcastStream();
    streams.state.listen(_stateController.add);
  }

  /// The reactive row stream for [table]: the full current row set,
  /// re-emitted immediately with the durable on-disk snapshot (visible
  /// offline, before any network event) and again after every applied
  /// change. [table] must match the table passed to the most recent
  /// [subscribe] call.
  ///
  /// Throws [StateError] if [subscribe] for [table] hasn't been called.
  Stream<List<Map<String, dynamic>>> watch(String table) {
    final rows = _rowsStream;
    if (_subscribedTable != table || rows == null) {
      throw StateError(
        'watch("$table") called without a matching subscribe("$table") '
        'first. nostos_flutter v1 supports one active subscription per Nostos '
        'instance (currently: ${_subscribedTable == null ? "none" : '"$_subscribedTable"'}).',
      );
    }
    return rows;
  }

  /// Reactive SQL watch (PowerSync parity P1). Re-runs [sql] whenever the
  /// synced data changes (the same change-tick [watch] pumps) and emits the
  /// decoded result set. Requires an active subscription first (v1: one
  /// table per `Nostos` instance — see the class doc). `sql` typically uses
  /// `json_extract(payload, '$.col')` against the synced `cairn_data` table
  /// (JSON1 ships in the bundled SQLite; ADR-0019).
  ///
  /// Unlike [watch] (which emits the full subscribed row set verbatim), this
  /// lets the caller project / filter / join with arbitrary SQL — at the
  /// cost of a fresh `SELECT` per tick.
  ///
  /// Optional PowerSync-parity refinements (audit table "Full-parity audit"
  /// in docs/plans/powersync-sdk-parity-plan.md):
  /// - [triggerOnTables] — tables whose mutation should re-run [sql].
  ///   Defaults to the subscribed table. v1 is one-table-per-handle, so
  ///   every entry MUST equal the subscribed table; any other name throws
  ///   [ArgumentError] (the caller is asking for notifications this
  ///   instance can never deliver).
  /// - [throttle] — coalesce a burst of change-ticks into at most one
  ///   re-query per window (trailing-edge debounce). null (the default)
  ///   preserves the original one-re-query-per-tick behavior. The debounce
  ///   sits BEFORE the `asyncMap` that runs the SQL, so it bounds the
  ///   actual `_engine.query` call rate — not just the downstream emit
  ///   rate.
  Stream<List<Map<String, dynamic>>> watchQuery(
    String sql, {
    List<String>? triggerOnTables,
    Duration? throttle,
  }) {
    final rows = _rowsStream;
    if (rows == null) {
      throw StateError(
        'watchQuery("$sql") called without subscribe() first. nostos_flutter '
        'v1 supports one active subscription per Nostos instance.',
      );
    }
    // PowerSync `triggerOnTables` parity: validate the requested triggers
    // against the active subscription. nostos_flutter v1 is one-table-per-
    // handle, so the only legal entry is the subscribed table itself — a
    // caller asking for any other table is requesting notifications this
    // instance can never deliver, so fail loudly.
    final activeTable = _subscribedTable!;
    final triggers = triggerOnTables ?? [activeTable];
    for (final t in triggers) {
      if (t != activeTable) {
        throw ArgumentError(
          'triggerOnTables contains "$t", but this Nostos instance is '
          'subscribed to "$activeTable". nostos_flutter v1 supports one '
          'active subscription per instance; every triggerOnTables entry '
          'must match it.',
        );
      }
    }

    // ponytail: with one-table-per-handle every tick on `rows` is already
    // for `activeTable`, so `triggers` can't actually filter here yet.
    // Once P5 (Sync Streams) lands multi-table sessions and ticks carry a
    // source-table tag, re-derive this line into
    // `rows.where((tick) => triggers.contains(tick.sourceTable))` and the
    // filter becomes load-bearing. Until then `triggers` is purely an
    // API-shape + validation check; `throttle` is the only knob here that
    // changes runtime behavior.
    final Stream<List<Map<String, dynamic>>> ticks =
        throttle == null ? rows : _debounceTicks(rows, throttle);

    return ticks.asyncMap((_) async =>
        (jsonDecode(await _engine.query(sql: sql)) as List<dynamic>)
            .cast<Map<String, dynamic>>());
  }

  /// Enqueue a durable write. Returns the local outbox id once the write is
  /// captured on disk — NOT once the server acks it; the applied row
  /// round-trips back through [watch] like any other replicated change (see
  /// `nostos-client`'s ADR-0013 outbox contract). `op` is one of:
  /// - `"upsert"` — insert-or-update (full row image in [payload]).
  /// - `"delete"` — delete by primary key.
  /// - `"patch"` — column-level UPDATE of an existing row; [payload] carries
  ///   ONLY the columns to change, columns absent are untouched, and the row
  ///   is never inserted (P3 PowerSync PATCH parity).
  ///
  /// [table] must match the active subscription (v1 constraint — see the
  /// class doc).
  Future<int> write(
    String table, {
    required String op,
    required String pk,
    Map<String, dynamic>? payload,
  }) {
    if (_subscribedTable != table) {
      throw StateError(
        'write("$table", ...) does not match the active subscription '
        '(${_subscribedTable == null ? "none — call subscribe() first" : '"$_subscribedTable"'}).',
      );
    }
    return _engine.write(
      table: table,
      op: op,
      pk: pk,
      payloadJson: payload == null ? null : jsonEncode(payload),
    );
  }

  /// Tears down the background sync loop and watch-stream pump for the
  /// active subscription (if any) — call this from a widget's own
  /// `dispose()` lifecycle method so a torn-down UI doesn't leave a
  /// WebSocket connection and reconnect loop running forever. Safe to call
  /// with no subscription, and safe to call more than once.
  ///
  /// Does not delete the local SQLite file, and does not prevent calling
  /// [subscribe] again on this same instance to resume from it — but once
  /// [close] has run, [connectionState] stops emitting (its underlying
  /// controller is closed), so a caller that wants to keep observing state
  /// after a fresh [subscribe] should create a new [Nostos] instead.
  Future<void> close() async {
    await _engine.close();
    await _stateController.close();
  }

  static List<Map<String, dynamic>> _decodeRows(String jsonArray) {
    final decoded = jsonDecode(jsonArray) as List<dynamic>;
    return decoded.cast<Map<String, dynamic>>();
  }

  /// Trailing-edge debounce on the change-tick stream: each tick within
  /// `duration` of the previous resets the timer; only the last tick of a
  /// burst propagates. Used by [watchQuery] to coalesce ticks BEFORE the
  /// `asyncMap` that runs the SQL, bounding `_engine.query` calls per
  /// throttle window (PowerSync `throttle` contract — see
  /// docs/plans/powersync-sdk-parity-plan.md).
  ///
  /// Single-subscription: matches `asyncMap`'s per-call semantics (each
  /// `watchQuery()` caller gets its own debounce pipeline). Cancelling the
  /// only listener tears down both the timer and the upstream subscription.
  static Stream<List<Map<String, dynamic>>> _debounceTicks(
    Stream<List<Map<String, dynamic>>> source,
    Duration duration,
  ) {
    Timer? timer;
    List<Map<String, dynamic>>? latest;
    bool hasPending = false;
    StreamSubscription<List<Map<String, dynamic>>>? sub;
    late final StreamController<List<Map<String, dynamic>>> controller;
    controller = StreamController<List<Map<String, dynamic>>>(
      onListen: () {
        sub = source.listen(
          (event) {
            latest = event;
            hasPending = true;
            timer?.cancel();
            timer = Timer(duration, () {
              if (!hasPending) return;
              final pending = latest;
              hasPending = false;
              if (pending != null) controller.add(pending);
            });
          },
          onError: controller.addError,
          onDone: () {
            timer?.cancel();
            if (hasPending) {
              final pending = latest;
              hasPending = false;
              if (pending != null) controller.add(pending);
            }
            controller.close();
          },
        );
      },
      onCancel: () {
        timer?.cancel();
        return sub?.cancel();
      },
    );
    return controller.stream;
  }

  static Future<String> _defaultSqlitePath(String url) async {
    final dir = await getApplicationSupportDirectory();
    final safeName = url.replaceAll(RegExp(r'[^A-Za-z0-9]+'), '_');
    return '${dir.path}/nostos_$safeName.sqlite';
  }
}

/// Convenience wrapper for Supabase-backed projects: connect using the
/// current session's access token instead of hand-rolling a
/// [Nostos.connect] call. Does **not** depend on the `supabase_flutter`
/// package — pass the token from whatever auth source you use.
///
/// For `supabase_flutter` users, the one-liner is:
/// ```dart
/// final session = Supabase.instance.client.auth.currentSession!;
/// final nostos = await NostosSupabase.connect(
///   nostosUrl: 'ws://localhost:8800/sync', // your `nostos dev` URL
///   supabaseUrl: 'https://<project-ref>.supabase.co',
///   accessToken: session.accessToken,
/// );
/// ```
/// `supabase_flutter`'s `onAuthStateChange` fires on token refresh; re-call
/// `NostosSupabase.connect` (or re-`subscribe`) with the new token when it
/// does — auto-refresh pass-through is not yet wired transparently (v1).
class NostosSupabase {
  const NostosSupabase._();

  /// [nostosUrl] is where your `nostos-server` `/sync` endpoint actually runs
  /// (`nostos dev` prints it). [supabaseUrl] is accepted and threaded through
  /// for forward-compatibility + documentation clarity but not yet used to
  /// derive anything.
  ///
  /// ponytail: once the `nostos` CLI (W3: `nostos init`/`nostos dev`) publishes
  /// a well-known discovery endpoint tied to the Supabase project, this
  /// could auto-derive `nostosUrl` from `supabaseUrl` and drop the extra
  /// parameter. Until then it's explicit, not magic.
  static Future<Nostos> connect({
    required String nostosUrl,
    required String supabaseUrl,
    required String accessToken,
    String? sqlitePath,
  }) {
    return Nostos.connect(url: nostosUrl, token: accessToken, sqlitePath: sqlitePath);
  }
}
