import 'dart:async';
import 'dart:convert';

import 'package:meta/meta.dart';

import 'engine.dart';
import 'engine_selector.dart';

export 'engine.dart' show NostosConnectionState, NostosTableSub;

/// A live Nostos sync connection.
///
/// No connector class, no client-side schema artifact: rows are plain
/// `Map<String, dynamic>` decoded from the same JSON payload the server
/// delivers (or `PgReplicator` produces from Postgres). Rust owns SQLite and
/// the sync loop; this class is a thin, reactive Dart wrapper.
///
/// Supports **multiple tables per `Nostos` instance** over one `/sync`
/// WebSocket (D1/ADR-0022): one resume LSN, one checkpoint, one ack stream.
/// Call [subscribeTables] once with the full table set, then [watch] per
/// table to receive its row stream. Calling subscribe again replaces the
/// previous subscription (its background connection + watch pumps are torn
/// down). The single-table [subscribe] is a convenience that subscribes to
/// exactly one table.
class Nostos {
  Nostos._(this._engine, {Set<String>? orSetTables, Set<String>? counterTables})
    : _orSetTables = orSetTables ?? const <String>{},
      _counterTables = counterTables ?? const <String>{};

  /// Test-only constructor: inject a fake [NostosEngine] to exercise this
  /// class's wiring (subscribe/watch/write, table-mismatch errors, JSON
  /// decode fallback) without the native library. See `test/nostos_test.dart`.
  /// [orSetTables] / [counterTables] mirror the [connect] declarations so a
  /// fake-level test can prove the CRDT tiers reach `engine.subscribe`.
  @visibleForTesting
  Nostos.withEngine(
    NostosEngine engine, {
    Set<String>? orSetTables,
    Set<String>? counterTables,
  }) : this._(
          engine,
          orSetTables: orSetTables,
          counterTables: counterTables,
        );

  final NostosEngine _engine;

  /// Tables tagged as add-wins OR-sets / PN-Counters (ADR-0030 / ADR-0032 T4),
  /// declared at [connect] and forwarded into every [subscribeTables] so
  /// `orSet*` / `counter*` verbs merge instead of throwing `*TableNotTagged`.
  final Set<String> _orSetTables;
  final Set<String> _counterTables;

  /// Open a connection to a `nostos-server` `/sync` endpoint. Does not touch
  /// the network yet — [subscribe] starts the actual session.
  ///
  /// [url] is the WebSocket URL, e.g. `ws://localhost:8800/sync` (see
  /// `nostos dev`'s printed URL). [token] is a bearer token sent as `?token=`
  /// on the WS handshake (matches whatever `NOSTOS_SYNC_AUTH` mode the server
  /// runs — `none` ignores it, `supabase-jwt` verifies it).
  ///
  /// Platform selection (ADR-0036) happens here via a compile-time conditional
  /// import ([createNostosEngine]): native → [RustNostosEngine] (frb + the Rust
  /// dylib + a `path_provider` SQLite path); web → [WebNostosEngine] over the
  /// shared `nostos-ffi-wasm` Worker (opfs-sahpool). [sqlitePath] is native-only
  /// (web durability is OPFS-backed); [workerUrl] overrides the web Worker
  /// script URL (default `nostos/nostos_worker.js`).
  static Future<Nostos> connect({
    required String url,
    String? token,
    String? sqlitePath,
    String? workerUrl,
    Set<String>? orSetTables,
    Set<String>? counterTables,
  }) async {
    return Nostos._(
      await createNostosEngine(
        url: url,
        token: token,
        sqlitePath: sqlitePath,
        workerUrl: workerUrl,
      ),
      orSetTables: orSetTables,
      counterTables: counterTables,
    );
  }

  /// The set of tables the active subscription covers (empty before the first
  /// subscribe). Drives the [watch]/[write] membership checks.
  final Set<String> _subscribedTables = {};

  /// Lazy per-table decoded row streams. [watch] populates this on first call
  /// per table; cleared on a new subscribe (which tears down the pumps).
  final Map<String, Stream<List<Map<String, dynamic>>>> _watchCache = {};

  final StreamController<NostosConnectionState> _stateController =
      StreamController<NostosConnectionState>.broadcast();

  /// Connection-state transitions for the current (or most recently started)
  /// subscription. Empty until [subscribe] has been called at least once.
  Stream<NostosConnectionState> get connectionState => _stateController.stream;

  /// Durable-outbox status (queued / permanently-failed writes). Prefer
  /// `NostosDatabase.status`, which folds this into [SyncStatus] alongside the
  /// connection state; this is the raw stream for apps using [Nostos] directly.
  ///
  /// Requires a prior [subscribe]. Emits the current value on listen.
  Stream<({int pending, int deadLettered, String? lastError})>
  get writeStatus => _engine.watchWriteStatus();

  /// Web-only storage degrade signal (folds into [SyncStatus.webStorageDegraded]
  /// via NostosDatabase). Native never fires. See [NostosEngine.webStorageDegraded].
  Stream<bool> get webStorageDegraded => _engine.webStorageDegraded;

  /// Materialize the WS2 read-views for [tables] in the on-device SQLite
  /// file (`CREATE VIEW IF NOT EXISTS <table> AS SELECT json_extract(...)
  /// AS col, ... FROM cairn_data WHERE table_name='<table>'` — see
  /// `SqliteStorage::apply_schema`). Idempotent for an unchanged schema;
  /// the views persist in the SQLite file, so this only needs to run once
  /// after [connect] and before the first [watch] / [watchQuery] / [getAll]
  /// against a projected `<table>`. Synchronous (the FFI is `Result<(),
  /// String>` and throws on error). Most apps won't call this directly —
  /// `NostosDatabase.connect` wires it from a resolved `NostosSchema`.
  void applySchema(List<ClientTableFfi> tables) => _engine.applySchema(tables);

  /// Subscribe to [tables] over one `/sync` socket (D1/ADR-0022 multi-table).
  /// Each entry may carry its own [NostosTableSub.whereSql] (a safe-SQL
  /// predicate, ADR-0012). Replaces any previous subscription on this
  /// instance (its background connection + watch pumps are torn down).
  Future<void> subscribeTables(List<NostosTableSub> tables) async {
    if (tables.isEmpty) {
      throw ArgumentError('subscribeTables() requires at least one table');
    }
    _watchCache.clear();
    _subscribedTables
      ..clear()
      ..addAll(tables.map((t) => t.name));
    _engine
        .subscribe(
          tables: tables,
          orSetTables: _orSetTables,
          counterTables: _counterTables,
        )
        .listen(_stateController.add);
  }

  /// Single-table convenience — equivalent to
  /// `subscribeTables([NostosTableSub(name: table, whereSql: where)])`.
  Future<void> subscribe(String table, {String? where}) =>
      subscribeTables([NostosTableSub(name: table, whereSql: where)]);

  /// P5 sync streams (docs/plans/p5-sync-streams-design.md), PowerSync-shaped:
  /// `db.syncStream('lists', {'owner': uid}).subscribe()`. The stream is
  /// server-defined (`[streams.<name>]` in `nostos_rules.toml`) and
  /// client-parameterized — [params] binds the template's `:param`
  /// placeholders value-level (never textual; injection-safe by
  /// construction). Lazy: subscribing does NOT tear down the session; the
  /// frame rides the live socket or the next reconnect. Rows surface through
  /// the existing [watch]/[watchQuery] layer — streams control which rows
  /// land in SQLite.
  NostosSyncStream syncStream(String name, [Map<String, dynamic> params = const {}]) =>
      NostosSyncStream._(_engine, name, params);

  /// The reactive row stream for [table]: the full current row set, re-emitted
  /// immediately with the durable on-disk snapshot (visible offline, before
  /// any network event) and again after every applied change. [table] must be
  /// among those passed to [subscribeTables]. Lazily attaches a per-table
  /// engine watch on first call (cached); subsequent calls share the stream.
  ///
  /// Throws [StateError] if [table] is not in the active subscription.
  Stream<List<Map<String, dynamic>>> watch(String table) {
    if (!_subscribedTables.contains(table)) {
      throw StateError(
        'watch("$table") called without that table in the active subscription. '
        'Subscribed tables: '
        '${_subscribedTables.isEmpty ? "(none — call subscribe first)" : _subscribedTables.toList()}.',
      );
    }
    return _watchCache.putIfAbsent(
      table,
      // Replay the latest row set to each new subscriber. A plain
      // `.asBroadcastStream()` does NOT replay, so a StreamBuilder that mounts
      // after the engine's initial snapshot tick (e.g. when a NavigationRail
      // page is rebuilt on tab switch) sees `connectionState: waiting` with no
      // data and renders empty — "No providers yet." while the rows are on
      // disk. Caching + replaying the last emission fixes this: every later
      // subscriber immediately gets the current rows, then live updates.
      () => _replayLatest(_engine.watch(table: table).map(_decodeRows)),
    );
  }

  /// Convert a single-subscription stream into a broadcast stream that REPLAYS
  /// the most recent event to each new listener (a "BehaviorSubject"/
  /// `publishValue` equivalent without a dep). Used by [watch] so pages rebuilt
  /// on navigation still see the current row set. While at least one listener
  /// is attached the upstream subscription stays live; the upstream is
  /// cancelled only when the last listener cancels AND no cached value is held.
  static Stream<List<Map<String, dynamic>>> _replayLatest(
    Stream<List<Map<String, dynamic>>> source,
  ) {
    List<Map<String, dynamic>>? latest;
    StreamSubscription<List<Map<String, dynamic>>>? sub;
    final controller = StreamController<List<Map<String, dynamic>>>.broadcast();
    void ensureListening() {
      if (sub != null) return;
      sub = source.listen(
        (event) {
          latest = event;
          controller.add(event);
        },
        onError: controller.addError,
        onDone: controller.close,
      );
    }

    controller.onListen = () {
      ensureListening();
      // Replay the cached value (if any) the instant a new listener attaches.
      if (latest != null) {
        // Emit asynchronously so a listener added during build receives the
        // event in the next microtask, not synchronously mid-subscribe.
        scheduleMicrotask(() => controller.add(List.of(latest!)));
      }
    };
    controller.onCancel = () {
      // Keep `sub` alive as long as we hold a cached value: a future listener
      // must receive `latest` on subscribe, and the upstream must keep feeding
      // live updates. Only tear down when there is nothing to replay.
      if (latest == null) {
        sub?.cancel();
        sub = null;
      }
    };
    return controller.stream;
  }

  /// Run an arbitrary SELECT against on-device SQLite once (non-reactive).
  /// Returns the raw JSON-array-of-objects string straight from the engine;
  /// decode with `jsonDecode`. Requires an active subscription (the engine
  /// enforces this — same contract as [watchQuery]). This is the one-shot
  /// counterpart to [watchQuery]'s reactive stream; `NostosDatabase.getAll`
  /// routes through here.
  Future<String> query(String sql) => _engine.query(sql: sql);

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
    if (_subscribedTables.isEmpty) {
      throw StateError('watchQuery("$sql") called without subscribe() first.');
    }
    // PowerSync `triggerOnTables` parity: validate against the subscribed set.
    // Multi-table (D1/ADR-0022): any subscribed table is a legal trigger
    // (default = all subscribed). A name outside the set can never fire, so
    // fail loudly.
    final triggers = triggerOnTables ?? _subscribedTables.toList();
    for (final t in triggers) {
      if (!_subscribedTables.contains(t)) {
        throw ArgumentError(
          'triggerOnTables contains "$t", which is not in the subscribed set '
          '(${_subscribedTables.toList()}). Every trigger must be subscribed.',
        );
      }
    }
    // Tick source: merge each trigger table's (cached) watch stream — a change
    // to ANY trigger table re-runs the SQL.
    final merged = _mergeTriggers(triggers.map(watch).toList(growable: false));
    final Stream<List<Map<String, dynamic>>> ticks = throttle == null
        ? merged
        : _debounceTicks(merged, throttle);

    return ticks.asyncMap(
      (_) async {
        final rows = (jsonDecode(await _engine.query(sql: sql)) as List<dynamic>)
            .cast<Map<String, dynamic>>()
            .toList();
        return rows;
      },
    );
  }

  /// Reactive typed-record watch (WS6, opt-in). Like [watchQuery] but decodes
  /// each row into a typed record via [fromRow]. PowerSync parity: PowerSync
  /// returns untyped `Map` rows and documents a user-written `fromRow`; this
  /// folds the `.map(fromRow)` boilerplate into the call. Requires an active
  /// subscription first (see [watchQuery]).
  ///
  /// [fromRow] is YOUR mapping function — typically a record's `fromRow`/
  /// `fromJson` factory. It runs per row per tick, so keep it allocation-light.
  /// The schema's column affinity (TEXT→String, INTEGER→int, REAL→double) is
  /// the right cast guide when the schema was server-fetched.
  Stream<List<T>> watchMapped<T>(
    String sql,
    T Function(Map<String, dynamic> row) fromRow, {
    List<String>? triggerOnTables,
    Duration? throttle,
  }) => watchQuery(
    sql,
    triggerOnTables: triggerOnTables,
    throttle: throttle,
  ).map((rows) => rows.map(fromRow).toList(growable: false));

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
  /// [table] must be in the active subscription (see the class doc).
  Future<int> write(
    String table, {
    required String op,
    required String pk,
    Map<String, dynamic>? payload,
  }) {
    if (!_subscribedTables.contains(table)) {
      throw StateError(
        'write("$table", ...) is not in the active subscription '
        '(${_subscribedTables.isEmpty ? "none — call subscribe() first" : _subscribedTables.toList()}).',
      );
    }
    return _engine.write(
      table: table,
      op: op,
      pk: pk,
      payloadJson: payload == null ? null : jsonEncode(payload),
    );
  }

  /// Atomic batch enqueue — all ops land in one storage transaction or none do
  /// (ADR-0032 T3). Validates every op's table is in the active subscription
  /// and JSON-encodes payloads BEFORE calling the engine, so a bad table fails
  /// fast with a clear error instead of a rolled-back FFI call. Returns outbox
  /// ids in the same order as [writes].
  Future<List<int>> writeBatch(
    List<({String table, String op, String pk, Map<String, dynamic>? payload})>
    writes,
  ) {
    for (final w in writes) {
      if (!_subscribedTables.contains(w.table)) {
        throw StateError(
          'writeBatch op (${w.table}/${w.op}/${w.pk}) is not in the active '
          'subscription '
          '(${_subscribedTables.isEmpty ? "none — call subscribe() first" : _subscribedTables.toList()}).',
        );
      }
    }
    return _engine.writeBatch(
      ops: writes
          .map(
            (w) => (
              table: w.table,
              op: w.op,
              pk: w.pk,
              payloadJson: w.payload == null ? null : jsonEncode(w.payload),
            ),
          )
          .toList(),
    );
  }

  /// Add [element] to the add-wins OR-set in row [pk] of [table] (ADR-0030 /
  /// ADR-0032 T4). Mints a client HLC and enqueues a merge-upsert; the element
  /// renders locally immediately and converges with concurrent remote adds on
  /// the server's echo. Requires an active subscription including [table].
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) {
    if (!_subscribedTables.contains(table)) {
      throw StateError(
        'orSetAdd("$table", ...) is not in the active subscription '
        '(${_subscribedTables.isEmpty ? "none — call subscribe() first" : _subscribedTables.toList()}).',
      );
    }
    return _engine.orSetAdd(table: table, pk: pk, element: element);
  }

  /// Remove [element] from the OR-set in row [pk] of [table] — a tombstone at
  /// a fresh HLC. Add-wins: a concurrent or later re-add revives the element.
  /// Requires an active subscription including [table].
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) {
    if (!_subscribedTables.contains(table)) {
      throw StateError(
        'orSetRemove("$table", ...) is not in the active subscription '
        '(${_subscribedTables.isEmpty ? "none — call subscribe() first" : _subscribedTables.toList()}).',
      );
    }
    return _engine.orSetRemove(table: table, pk: pk, element: element);
  }

  /// Increment the PN-Counter in row [pk] of [table] by [delta] (ADR-0030
  /// addendum). Read-modify-write: reads the current counter payload, applies
  /// the delta to this replica's entry, and enqueues the result. Converges with
  /// concurrent remote increments on the server's echo. Requires an active
  /// subscription including [table].
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) {
    if (!_subscribedTables.contains(table)) {
      throw StateError(
        'counterIncrement("$table", ...) is not in the active subscription '
        '(${_subscribedTables.isEmpty ? "none — call subscribe() first" : _subscribedTables.toList()}).',
      );
    }
    return _engine.counterIncrement(table: table, pk: pk, delta: delta);
  }

  /// Decrement the PN-Counter by [delta] (bumps the negative counter `n`).
  /// Requires an active subscription including [table].
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) {
    if (!_subscribedTables.contains(table)) {
      throw StateError(
        'counterDecrement("$table", ...) is not in the active subscription '
        '(${_subscribedTables.isEmpty ? "none — call subscribe() first" : _subscribedTables.toList()}).',
      );
    }
    return _engine.counterDecrement(table: table, pk: pk, delta: delta);
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
  /// Replace the bearer token used for subsequent connections.
  ///
  /// Call this whenever your auth provider rotates the access token. For
  /// `supabase_flutter` that is `onAuthStateChange` emitting `tokenRefreshed` —
  /// and [NostosDatabase.supabase] wires this up for you, so you only need this
  /// directly if you manage auth yourself.
  ///
  /// **Why it matters:** a Supabase JWT expires in roughly an hour and the server
  /// enforces `exp`. Without a refresh the reconnect loop re-sends the dead token
  /// forever — the app keeps rendering local rows and never syncs again, with no
  /// error surfaced beyond the connection state flapping.
  ///
  /// Safe mid-session and idempotent. Nothing is torn down: no reconnect is
  /// forced, [watch] streams stay open, and the durable outbox is untouched. A
  /// live socket keeps running until it drops naturally; if the client is already
  /// retrying, the next attempt uses the new token, so a refresh self-heals
  /// within one backoff window.
  Future<void> setToken(String? token) => _engine.setToken(token);

  Future<void> close() async {
    await _engine.close();
    await _stateController.close();
  }

  /// ADR-0029: sign out — wipe local rows + durable outbox (so the next
  /// principal sees nothing of this one), stop sync, and clear the seed token.
  /// Unlike [close], this wipes the on-device SQLite state via
  /// `clear_local_state` before tearing the session down. Idempotent.
  Future<void> signOut() async {
    await _engine.signOut();
    await _stateController.close();
  }

  /// Pause syncing: abort ONLY the background connect loop, keeping the client,
  /// its on-device SQLite store, and every `watch()` pump alive. Reads, writes
  /// (enqueued to the durable outbox), and the UI keep working offline. Emits
  /// `disconnected` on [connectionState]. Idempotent (no-op if already paused or
  /// never subscribed).
  Future<void> disconnect() async {
    await _engine.disconnect();
    _stateController.add(NostosConnectionState.disconnected);
  }

  /// Resume syncing after [disconnect]: respawn the connect loop on the same
  /// client; the durable outbox flushes on reconnect and live updates resume.
  /// Emits `connecting → connected …` on [connectionState]. Requires a prior
  /// [subscribe] (throws otherwise). Named `resume`, not `connect`, to avoid
  /// clashing with the `static Nostos.connect` constructor.
  void resume() {
    _engine.resume().listen(_stateController.add);
  }

  static List<Map<String, dynamic>> _decodeRows(String jsonArray) {
    final decoded = jsonDecode(jsonArray) as List<dynamic>;
    return decoded.cast<Map<String, dynamic>>();
  }

  /// Merge N row streams into one: emits on ANY upstream tick (forwarding
  /// that tick's rows). Used by [watchQuery] to re-run SQL when any trigger
  /// table changes. Each upstream is a broadcast stream (from [watch]), so
  /// multiple watchQuery callers + direct watch() listeners all share the
  /// same underlying pumps.
  ///
  /// Subscribes to upstreams LAZILY (on the first downstream listener), not at
  /// construction. Eager subscription was a P0 bug: it forwarded each source's
  /// initial snapshot into this broadcast controller before the downstream
  /// `StreamBuilder` had subscribed (it mounts on the next frame), so the
  /// snapshot was dropped — the UI rendered "No providers yet." even with rows
  /// on disk. Each `watch(t)` source is already wrapped in [_replayLatest], so
  /// subscribing lazily means the cached snapshot is replayed to the real
  /// listener when it attaches. See `nostos-soundness-audit-2026-07-19.md` P0-3.
  static Stream<List<Map<String, dynamic>>> _mergeTriggers(
    List<Stream<List<Map<String, dynamic>>>> sources,
  ) {
    if (sources.length == 1) return sources.single;
    final controller = StreamController<List<Map<String, dynamic>>>.broadcast();
    final subs = <StreamSubscription<List<Map<String, dynamic>>>>[];
    controller.onListen = () {
      if (subs.isNotEmpty) return; // already wired (re-listen after cancel)
      for (final s in sources) {
        subs.add(s.listen(controller.add, onError: controller.addError));
      }
    };
    controller.onCancel = () {
      for (final sub in subs) {
        sub.cancel();
      }
      subs.clear();
    };
    return controller.stream;
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
/// `supabase_flutter`'s `onAuthStateChange` fires on token refresh. Call
/// [Nostos.setToken] with the new token when it does — that swaps the credential
/// in place without tearing anything down, so open `watch` streams survive.
/// (Do **not** re-`connect` for this: a fresh handle ends every stream the UI
/// holds.) If you use [NostosDatabase.supabase] instead of this wrapper, that
/// wiring is already done for you.
///
/// Corrected 2026-07-30: this previously said auto-refresh was "not yet wired
/// transparently (v1)" and advised re-calling `connect`. It is now wired in
/// `NostosDatabase.supabase`, and re-connecting was never the right advice.
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
    return Nostos.connect(
      url: nostosUrl,
      token: accessToken,
      sqlitePath: sqlitePath,
    );
  }
}

/// A declared sync stream (P5) — the PowerSync `syncStream(name, params)`
/// shape. Call [subscribe] to activate it on the live session.
class NostosSyncStream {
  const NostosSyncStream._(this._engine, this.name, this.params);

  final NostosEngine _engine;

  /// The server-defined stream name (`[streams.<name>]` in nostos_rules.toml).
  final String name;

  /// Bind values for the template's `:param` placeholders (JSON scalars).
  final Map<String, dynamic> params;

  /// Activate the stream: the `subscribe_stream` frame goes out on the live
  /// socket (or the next reconnect), and the stream re-subscribes on every
  /// reconnect until the returned subscription's [NostosStreamSubscription.unsubscribe].
  Future<NostosStreamSubscription> subscribe() async {
    final id = await _engine.subscribeStream(
      name: name,
      paramsJson: jsonEncode(params),
    );
    return NostosStreamSubscription._(_engine, id);
  }
}

/// A live sync-stream subscription (P5). [unsubscribe] stops the flow; v1
/// leaves local rows in place (eviction is separate; PowerSync behaves the
/// same).
class NostosStreamSubscription {
  const NostosStreamSubscription._(this._engine, this.id);

  final NostosEngine _engine;

  /// The client-chosen stream id (matches the `subscribe_stream` frame and
  /// the `stream` key on this stream's snapshot boundary frames).
  final String id;

  /// Unsubscribe now. Unknown/already-removed id = no-op (idempotent).
  Future<void> unsubscribe() => _engine.unsubscribeStream(id: id);
}
