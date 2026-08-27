import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:supabase_flutter/supabase_flutter.dart';

import 'nostos.dart';
import 'nostos_config.dart';
import 'predicate.dart';
import 'schema.dart';

/// PowerSync-style entry point: open a [Nostos] sync connection AND resolve
/// the server schema in one call, so `SELECT * FROM <table>` works
/// immediately against the WS2 read-views (see `SqliteStorage::apply_schema`
/// — the views persist in the SQLite file once [applySchema] runs).
///
/// The high-level DX:
/// ```dart
/// final db = await NostosDatabase.connect(
///   url: 'ws://localhost:8800/sync',
///   sqlitePath: './cairn.sqlite',
/// );
/// await db.subscribe('todos');
/// db.watch('SELECT * FROM todos').listen((rows) => print(rows));
/// await db.write(table: 'todos', op: 'upsert', pk: '1', payload: {'title': 'ship'});
/// ```
///
/// This class adds no sync logic of its own — it wires [Nostos] (the thin
/// reactive wrapper over the Rust engine) to a resolved [NostosSchema]. A
/// Supabase-flavored factory is provided (see [NostosDatabase.supabase]).
class NostosDatabase {
  NostosDatabase._(
    this._nostos,
    this.schema,
    this._httpBase,
    this._seedToken,
    this._supabaseAuth, {
    this._localOnly = false,
  }) {
    // ADR-0037 §3: every SDK deregisters its push tokens in its sign-out hook
    // — a leaked registration would push the previous principal's data to the
    // next user. Registered at construction (not inside registerPushToken) so
    // the hook exists even for a session that registers nothing.
    _signOutHooks.add(_deregisterPushTokensOnSignOut);
  }

  /// Whether this database was opened via [NostosDatabase.local] — no server,
  /// no sync, no push rail. Gates the fail-loudly guards on [resumeSync] and
  /// the push-token REST calls, and resolves [waitForFirstSync] immediately.
  final bool _localOnly;

  /// Test-only: wrap an injected [Nostos] (itself injectable via
  /// `Nostos.withEngine`) to exercise the typed mappers ([watchMapped] /
  /// [getAllMapped]) without the native library. See
  /// `test/nostos_ws6_test.dart`. [httpBase] / [token] feed the push-token
  /// REST seam (`test/push_token_test.dart` points them at a local server).
  @visibleForTesting
  factory NostosDatabase.forTest(
    Nostos nostos,
    NostosSchema schema, {
    String? httpBase,
    String? token,
    Future<String?> Function()? sessionRefresh,
  }) {
    final db = NostosDatabase._(nostos, schema, httpBase ?? '', token, false);
    db._sessionRefresh = sessionRefresh;
    return db;
  }

  final Nostos _nostos;

  /// The resolved server schema used to materialize the read-views.
  /// Exposed for inspection / codegen; not meant to be mutated.
  final NostosSchema schema;

  /// HTTP base for the REST surface (`GET /schema`, `POST /push-tokens`,
  /// `DELETE /push-tokens/{token}`), derived from the same WS [url] the sync
  /// connection uses — see [_deriveHttpBase].
  final String _httpBase;

  /// The explicit token passed to [connect], when auth wasn't Supabase. The
  /// push-token REST calls read this (or the live Supabase session, when
  /// [_supabaseAuth]) — the SAME credential source the sync connection uses;
  /// there is no second path. Mutable (L1): [signOut] clears it AFTER the
  /// sign-out hooks run — the push-token deregistration hook needs the seed
  /// to authorize its DELETEs — so any post-signOut REST call is anonymous.
  String? _seedToken;
  final bool _supabaseAuth;

  /// Re-acquires credentials when a REST call is rejected with 401 (a
  /// cold-restored Supabase session can serve an expired access token
  /// before the auto-refresh timer rotates it). Returns the fresh
  /// credential, or `null` when re-acquisition failed (no retry). Set for
  /// Supabase-authed instances in [_open]; `null` disables the retry.
  /// Injectable in tests via [forTest].
  Future<String?> Function()? _sessionRefresh;

  String? get _restAuthToken => _supabaseAuth
      ? Supabase.instance.client.auth.currentSession?.accessToken
      : _seedToken;

  /// Supabase auth listener forwarding token rotations (set by
  /// [NostosDatabase.supabase] only). MUST be cancelled in [close] — a surviving
  /// listener would call `setToken` on a closed engine on the next refresh.
  StreamSubscription<AuthState>? _authSub;

  /// Sign-out hooks — extra local state to wipe on [signOut] beyond the engine
  /// + outbox (ADR-0029). The T6 attachments driver registers its [BlobStore]
  /// wipe here so the next principal sees no blob bytes (consistent with the
  /// SQLite + outbox wipe). Hooks run AFTER the engine quiesces + wipes, are
  /// awaited in registration order, and MUST be idempotent (signOut is
  /// re-callable). A throwing hook is swallowed (best-effort) so one failing
  /// wipe cannot block the core sign-out.
  final List<Future<void> Function()> _signOutHooks =
      <Future<void> Function()>[];

  /// Open a [Nostos] connection and resolve the schema.
  ///
  /// [url] is the `nostos-server` `/sync` WebSocket URL (the one `nostos dev`
  /// prints). [sqlitePath] is the on-disk SQLite file location (no default
  /// here — callers choose it; pass the same path across runs to keep the
  /// durable store and its WS2 views).
  ///
  /// If [schema] is `null`, the HTTP base is derived from [url]
  /// (`wss`→`https`, `ws`→`http`, trailing path stripped) and `GET
  /// {base}/schema` is fetched + parsed via [NostosSchema.fromSchemaDescriptor].
  /// Then `Nostos.applySchema` runs once to create the read-views. Returns a
  /// ready [NostosDatabase]; call [subscribe] next to start syncing.
  ///
  /// Pass an explicit [schema] to skip the HTTP round-trip (e.g. a pinned
  /// schema bundled with the app, or a test fixture).
  static Future<NostosDatabase> connect({
    required String url,
    String? token,
    NostosSchema? schema,
    required String sqlitePath,
    Set<String>? orSetTables,
    Set<String>? counterTables,
  }) => _open(
    url: url,
    token: token,
    schema: schema,
    sqlitePath: sqlitePath,
    orSetTables: orSetTables,
    counterTables: counterTables,
  );

  /// Config-driven open: connect using a [NostosConfig] (normally loaded
  /// from the app's bundled `assets/nostos.json` via [NostosConfig.load])
  /// plus the app's declared [schema].
  ///
  /// This is the recommended app entry point:
  ///
  /// ```dart
  /// final config = await NostosConfig.load();
  /// final dir = await getApplicationSupportDirectory();
  /// final db = await NostosDatabase.open(
  ///   config: config,
  ///   schema: appSchema,
  ///   sqliteDir: dir.path,
  /// );
  /// ```
  ///
  /// Behavior:
  /// - SQLite lands at `{sqliteDir}/{config.sqliteFilename}`.
  /// - If [schema] is `null`, it is fetched from the server
  ///   (`GET {base}/schema`) as in [connect]. Passing your declared schema
  ///   is preferred — re-applying it at every connect IS the migration
  ///   mechanism (views are dropped + recreated; see [NostosSchema]).
  /// - If the config carries a `supabase` block, Supabase is initialized
  ///   (skipped when the app already called `Supabase.initialize`) and the
  ///   signed-in session's access token becomes the sync bearer token —
  ///   throws [StateError] when nobody is signed in (same contract as
  ///   [NostosDatabase.supabase]).
  static Future<NostosDatabase> open({
    required NostosConfig config,
    NostosSchema? schema,
    required String sqliteDir,
    Set<String>? orSetTables,
    Set<String>? counterTables,
  }) async {
    final sqlitePath = '$sqliteDir/${config.sqliteFilename}';
    String? token;
    if (config.hasSupabase) {
      final initialized = _supabaseInitialized();
      if (!initialized) {
        await Supabase.initialize(
          url: config.supabaseUrl!,
          publishableKey: config.supabaseAnonKey!,
        );
      }
      final session = Supabase.instance.client.auth.currentSession;
      if (session == null) {
        throw StateError(
          'nostos config has a "supabase" block but there is no live session '
          '— sign in before calling NostosDatabase.open()',
        );
      }
      token = session.accessToken;
    }
    return _open(
      url: config.url,
      token: token,
      schema: schema,
      sqlitePath: sqlitePath,
      orSetTables: orSetTables,
      counterTables: counterTables,
      supabaseAuth: config.hasSupabase,
    );
  }

  /// `Supabase.initialize` is process-global and once-only; probing
  /// [Supabase.instance] is the only supported "is it initialized?" check
  /// (it throws [AssertionError] before initialize).
  static bool _supabaseInitialized() {
    try {
      Supabase.instance;
      return true;
    } on AssertionError {
      return false;
    }
  }

  /// The `/sync` URL a [local] database is constructed with. Never dialed:
  /// [pauseSync] aborts the connect loop immediately after subscribe, and the
  /// loop's disconnect-gate check precedes any network attempt (a loop spawned
  /// after the gate is set returns a clean no-op session) — so this address
  /// only matters if a dial somehow slips the race, in which case the loopback
  /// discard port refuses instantly and nothing leaves the device.
  static const String _localPlaceholderUrl = 'ws://127.0.0.1:9/sync';

  /// Open a LOCAL-ONLY database: on-device SQLite + the durable outbox, with
  /// no server and no sync loop. Every nostos feature works identically —
  /// declared schema, typed reads, watches, writes (enqueued durably), CRDT
  /// verbs — except anything that by definition needs a server (sync,
  /// push-token REST), which fails loudly instead of silently no-opping.
  ///
  /// This is the free-tier parity entry point: a user without a database gets
  /// the full app on local storage, and upgrading to sync is a reopen of the
  /// SAME SQLite file with a real `/sync` URL ([connect]/[open]/[supabase]) —
  /// zero data migration.
  ///
  /// [schema] is REQUIRED (there is no server to fetch one from) and must
  /// declare at least one table; it is applied exactly as in [connect], so the
  /// read-views exist before the first watch/query. [sqliteDir] +
  /// [sqliteFilename] locate the durable store (same rule as [open]).
  /// [orSetTables] / [counterTables] tag the CRDT tiers exactly as in
  /// [connect].
  ///
  /// Semantics inherited from [pauseSync]: reads, writes, and `watch` pumps
  /// keep working while the socket is gone. Guards specific to this mode:
  /// [resumeSync] throws [StateError] (there is nothing to resume — reopen
  /// with a URL instead), the push-token calls throw [StateError] (there is
  /// no server to knock), and [waitForFirstSync] resolves immediately (there
  /// is no first sync to await).
  static Future<NostosDatabase> local({
    required String sqliteDir,
    String sqliteFilename = 'cairn.sqlite',
    required NostosSchema schema,
    Set<String>? orSetTables,
    Set<String>? counterTables,
  }) async {
    final nostos = await Nostos.connect(
      url: _localPlaceholderUrl,
      sqlitePath: '$sqliteDir/$sqliteFilename',
      orSetTables: orSetTables,
      counterTables: counterTables,
    );
    return _openLocal(nostos, schema);
  }

  /// Test seam for [local]: drives the exact local-open path around an
  /// injected [Nostos] (e.g. `Nostos.withEngine(fake)`), so pure-Dart tests pin
  /// the schema/apply/subscribe/pause ordering and the local-mode guards
  /// without the native library. See `test/local_database_test.dart`.
  @visibleForTesting
  static Future<NostosDatabase> localForTest(Nostos nostos, NostosSchema schema) =>
      _openLocal(nostos, schema);

  /// Shared local-open path behind [local] and [localForTest]: apply the
  /// declared schema, subscribe every declared table (so `watch` / `getAll` /
  /// `write` membership holds exactly as after a synced open), then pause the
  /// sync loop before it can dial.
  static Future<NostosDatabase> _openLocal(Nostos nostos, NostosSchema schema) async {
    if (schema.tables.isEmpty) {
      throw ArgumentError.value(
        schema.tables,
        'schema.tables',
        'NostosDatabase.local requires a declared schema with at least one '
            'table — there is no server to fetch one from',
      );
    }
    nostos.applySchema(schema.toClientTables());
    final db = NostosDatabase._(nostos, schema, '', null, false, localOnly: true);
    await db.subscribeTables([
      for (final table in schema.tables) NostosTableSub(name: table.name),
    ]);
    // Abort ONLY the connect loop; the client, the store, and every watch
    // pump stay alive (see pauseSync). The run task is aborted + reaped
    // (NostosHandle.disconnect), and the loop's gate check precedes any dial,
    // so the placeholder URL is never meaningfully contacted.
    await db.pauseSync();
    return db;
  }


  /// Open a [Nostos] connection for a Supabase-authenticated app.
  ///
  /// The caller MUST run `Supabase.initialize(...)` once at app start
  /// (before `runApp`) — `Supabase.initialize` is process-global and
  /// once-only, so this factory does NOT call it. The caller MUST also
  /// ensure a session exists (sign-in completed) before invoking this
  /// factory; the live session's `accessToken` is read via
  /// `Supabase.instance.client.auth.currentSession?.accessToken` and
  /// passed as the bearer token to the underlying [Nostos] connection.
  ///
  /// [nostosUrl] is the `nostos-server` `/sync` WebSocket URL. [sqlitePath]
  /// is the on-disk SQLite file location. [schema], if `null`, is fetched
  /// via `GET {httpBase}/schema` (see [connect] for the derivation rules).
  ///
  /// Throws [StateError] if there is no live Supabase session (the user
  /// must sign in before calling this factory).
  ///
  /// **Token refresh is handled for you** (since 2026-07-30). This factory
  /// subscribes to `Supabase.instance.client.auth.onAuthStateChange` and
  /// forwards rotated tokens into the sync client via [Nostos.setToken] — see
  /// [_wireSupabaseTokenRefresh]. [close] cancels that subscription.
  ///
  /// This used to say the token was read ONCE at connect time and that
  /// transparent refresh was a "v1 fast-follow", which undersold it: the
  /// consequence was that sync stopped roughly an hour after sign-in and never
  /// recovered, with nothing surfaced but a flapping connection state. It also
  /// pointed at "the token-swap primitive" in `NostosSupabase`, which did not
  /// exist — `NostosSupabase.connect` only forwards to `Nostos.connect`.
  ///
  /// Note the fix is deliberately NOT a reconnect: [Nostos.setToken] mutates the
  /// live token so the next connection uses it, leaving every `watch` stream
  /// open. Rebuilding the handle instead — the obvious pure-Dart approach —
  /// would end those streams and look to a user like data disappearing.
  static Future<NostosDatabase> supabase({
    required String nostosUrl,
    NostosSchema? schema,
    required String sqlitePath,
    Set<String>? orSetTables,
    Set<String>? counterTables,
  }) async {
    final session = Supabase.instance.client.auth.currentSession;
    if (session == null) {
      throw StateError(
        'no Supabase session — sign in before calling NostosDatabase.supabase()',
      );
    }
    final db = await _open(
      url: nostosUrl,
      token: session.accessToken,
      schema: schema,
      sqlitePath: sqlitePath,
      orSetTables: orSetTables,
      counterTables: counterTables,
      supabaseAuth: true,
    );
    db._wireSupabaseTokenRefresh();
    return db;
  }

  /// Forward Supabase token rotations into the sync client for the life of this
  /// database. Cancelled by [close].
  ///
  /// Without this, sync dies about an hour after sign-in and never recovers: the
  /// access token expires, the server rejects it on `exp`, and the reconnect loop
  /// re-sends the same dead credential indefinitely while the UI keeps rendering
  /// local rows. That failure is invisible apart from the connection state
  /// flapping, which is what made it worth fixing inside the factory rather than
  /// documenting as the caller's job.
  ///
  /// `signedIn` is handled as well as `tokenRefreshed` because
  /// `supabase_flutter` replays `signedIn` on session recovery at startup, and a
  /// recovered session can carry a token newer than the one we opened with.
  /// `signedOut` clears the token instead of leaving a stale credential in place.
  ///
  /// [Nostos.setToken] tears nothing down, so this never disturbs an open stream.
  void _wireSupabaseTokenRefresh() {
    _authSub = Supabase.instance.client.auth.onAuthStateChange.listen((data) {
      switch (data.event) {
        case AuthChangeEvent.tokenRefreshed:
        case AuthChangeEvent.signedIn:
        case AuthChangeEvent.userUpdated:
          final token = data.session?.accessToken;
          if (token != null) {
            // Fire-and-forget: the FFI call is cheap and a failure here must not
            // take down the auth stream (which would strand every later refresh).
            _nostos.setToken(token).catchError((Object _) {});
          }
        case AuthChangeEvent.signedOut:
          _nostos.setToken(null).catchError((Object _) {});
        default:
          break;
      }
    });
  }

  /// Shared open path for [connect] and [supabase]: open the [Nostos]
  /// connection, resolve the schema (passed or fetched), and apply it.
  /// Both factories delegate here so the connect/apply sequence has one
  /// home.
  static Future<NostosDatabase> _open({
    required String url,
    String? token,
    NostosSchema? schema,
    required String sqlitePath,
    Set<String>? orSetTables,
    Set<String>? counterTables,
    bool supabaseAuth = false,
  }) async {
    final nostos = await Nostos.connect(
      url: url,
      token: token,
      sqlitePath: sqlitePath,
      orSetTables: orSetTables,
      counterTables: counterTables,
    );
    final resolved = schema ?? await _fetchSchema(_deriveHttpBase(url));
    nostos.applySchema(resolved.toClientTables());
    final db = NostosDatabase._(
      nostos,
      resolved,
      _deriveHttpBase(url),
      token,
      supabaseAuth,
    );
    if (supabaseAuth) {
      // REST-side credential self-healing (see [_sessionRefresh]): the WS
      // side already rotates via _wireSupabaseTokenRefresh; this covers the
      // push-token REST seam.
      db._sessionRefresh = () async =>
          (await Supabase.instance.client.auth.refreshSession())
              .session
              ?.accessToken;
    }
    return db;
  }

  /// Connection-state transitions for the underlying [Nostos] session.
  Stream<NostosConnectionState> get connectionState => _nostos.connectionState;

  /// Snapshot of whether the session is currently `connected` (best-effort).
  /// True only after [status] has observed a `connected` transition; false
  /// before the first wire AND while `disconnected`. Used by the T6 attachment
  /// driver to gate blob transfers on connectivity (ADR-0034).
  bool get isOnline => _status?.value.conn == NostosConnectionState.connected;

  /// Subscribe to [table], optionally filtered by [where] (a safe-SQL
  /// predicate — see `Nostos.subscribe`). Must be called before [watch] /
  /// [getAll] / [write] for that table. For multiple tables on one
  /// connection, use [subscribeTables].
  Future<void> subscribe(String table, {String? where}) async {
    await _nostos.subscribe(table, where: where);
    _hasSubscribed = true;
    // Only if someone is already observing status — see [_wireWriteStatus]
    // for why the pump attaches at the LATER of first-status-access and
    // first-subscribe, never eagerly.
    if (_statusWired) _wireWriteStatus();
  }

  /// Subscribe to [tables] over one `/sync` socket (D1/ADR-0022 multi-table).
  /// Each entry may carry its own `whereSql`. Replaces any prior subscription.
  /// Call once with the full table set, then [watch] / [getAll] / [write] per
  /// table.
  Future<void> subscribeTables(List<NostosTableSub> tables) async {
    await _nostos.subscribeTables(tables);
    _hasSubscribed = true;
    if (_statusWired) _wireWriteStatus();
  }

  /// Reactive SQL watch (the ESCAPE HATCH — ADR-0032): re-runs [sql] whenever
  /// the synced data changes and emits the decoded result set. Thin delegate
  /// over `Nostos.watchQuery`. Requires an active [subscribe] first.
  ///
  /// This is the greppable raw-SQL escape hatch, kept for queries the typed
  /// `Collection<T>` + structured-predicate surface can't express yet (e.g. an
  /// `(col IS NULL) DESC` order, a join, or a projection). Prefer
  /// [Collection.watch] with [Where]/[Order] for every "table, maybe filter,
  /// maybe order" read — it is injection-safe by construction.
  Stream<List<Map<String, dynamic>>> watchSql(
    String sql, {
    Duration? throttle,
  }) => _nostos.watchQuery(sql, throttle: throttle);

  /// Legacy reactive SQL watch (alias of [watchSql]). Prefer [Collection.watch]
  /// (structured) for app reads; prefer [watchSql] if you must reach for raw
  /// SQL. Kept for back-compat with code written against the pre-contract
  /// surface.
  Stream<List<Map<String, dynamic>>> watch(String sql, {Duration? throttle}) =>
      watchSql(sql, throttle: throttle);

  /// Run a one-shot SELECT against on-device SQLite and return the decoded
  /// rows. Non-reactive counterpart to [watchSql]. Requires an active
  /// [subscribe] (the engine enforces this).
  Future<List<Map<String, dynamic>>> getAll(String sql) async =>
      (jsonDecode(await _nostos.query(sql)) as List<dynamic>)
          .cast<Map<String, dynamic>>();

  /// Raw-SQL execute (the ESCAPE HATCH — ADR-0032). A READ-ONLY alias of
  /// [getAll] — **by convention, not by enforcement.**
  ///
  /// Nothing here parses your SQL. `SqliteStorage::query` runs whatever it is
  /// handed, so a `DELETE` reaches SQLite and returns an empty result set.
  /// Two things keep that from corrupting state, and it is worth knowing which
  /// is which: statements aimed at a **synced table** fail loudly, with SQLite's
  /// `cannot modify ... because it is a view` (the read surface is a VIEW —
  /// ADR-0028), but statements aimed at an **internal** table are not
  /// protected, and `DELETE FROM cairn_outbox` would silently destroy queued
  /// writes. Do not route DML through here.
  ///
  /// ponytail: writes through raw SQL are a deliberate ceiling — add/delete/edit
  /// route through [write] / [Collection.upsert] / [Collection.patch] /
  /// [Collection.delete], which apply locally at once and round-trip the applied
  /// row back through [watch]. Accepting arbitrary INSERT/UPDATE/DELETE would
  /// bypass the outbox and desync local state from the replication stream.
  /// Upgrade path: parse raw SQL and route writes into [write].
  Future<List<Map<String, dynamic>>> execute(String sql) => getAll(sql);

  /// Reactive typed-record watch (WS6): like [watch] but maps each row to a
  /// typed record via [fromRow]. Thin delegate over `Nostos.watchMapped`.
  Stream<List<T>> watchMapped<T>(
    String sql,
    T Function(Map<String, dynamic> row) fromRow,
  ) => _nostos.watchMapped(sql, fromRow);

  /// One-shot typed-record query (WS6): like [getAll] but maps each row to a
  /// typed record via [fromRow].
  Future<List<T>> getAllMapped<T>(
    String sql,
    T Function(Map<String, dynamic> row) fromRow,
  ) async => (await getAll(sql)).map(fromRow).toList(growable: false);

  /// Enqueue a durable write into the local outbox. Returns the local outbox
  /// id (NOT a server ack — the applied row round-trips back through [watch];
  /// see `Nostos.write`). [op] is one of `"upsert"`, `"delete"`, `"patch"`.
  /// [table] must match the active subscription (v1).
  ///
  /// [payload] is the row image (for `upsert`) or column subset (for
  /// `patch`); it is JSON-encoded by `Nostos.write` before crossing FFI.
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    Map<String, dynamic>? payload,
  }) => _nostos.write(table, op: op, pk: pk, payload: payload);

  /// Enqueue a group of writes as an all-or-nothing *entry* batch
  /// (ADR-0032 T3). Every op enters the durable outbox atomically — all land in
  /// one SQLite transaction or none do — so the group uploads together in one
  /// round. Returns the local outbox ids in the same order as [writes].
  ///
  /// **This is NOT a server transaction.** The server applies each row
  /// individually with per-field last-writer-wins (ADR-0014); there is no
  /// cross-row rollback and no all-or-nothing *apply*. Two ops in the batch
  /// that touch the same row/field collapse to the last one's value (verified
  /// by the outbox's pk-dedup + the server's idempotent upsert) — exactly what
  /// a sequential `patch`/`upsert` sequence already produces.
  ///
  /// *Entry* atomicity IS real (one storage transaction): a mid-batch disk
  /// failure rolls back the whole batch, leaving zero partial outbox rows. The
  /// WASM engine (Wave 2) inherits the same contract via the Outbox trait's
  /// default `enqueue_batch` (sequential, best-effort) until it gains its own
  /// transactional override.
  Future<List<int>> writeBatch(List<NostosWrite> writes) async {
    if (writes.isEmpty) {
      throw ArgumentError.value(
        writes,
        'writes',
        'writeBatch requires a non-empty list',
      );
    }
    return _nostos.writeBatch(
      writes
          .map(
            (w) => (
              table: w.table,
              op: w.op,
              pk: w.pk.toString(),
              payload: w.payload,
            ),
          )
          .toList(),
    );
  }

  /// Read-only snapshot of the dead-letter queue (ADR-0032 T5 / ADR-0027):
  /// writes the server permanently rejected and the flush loop quarantined.
  /// Rows stay in `cairn_outbox` with `dlq = 1`; this lists them so failures are
  /// diagnosable. Each row carries the server's per-row reason ([DeadLetter]
  /// .error) and the quarantine timestamp ([DeadLetter.timestamp]). Order is
  /// oldest-first. v1 is read-only — `retryDeadLetter(id)` /
  /// `discardDeadLetter(id)` are deferred to v1.1.
  Future<List<DeadLetter>> deadLetters() async {
    final rows = await getAll(
      // Bare SQLite over the internal outbox table (read-only SELECT; the
      // `execute`-writes-by-convention warning does not apply to a SELECT).
      'SELECT id, table_name, op, pk, payload, attempts, last_error, '
      'dead_lettered_at '
      'FROM cairn_outbox WHERE dlq = 1 ORDER BY id ASC',
    );
    return rows
        .map((r) {
          final payloadJson = r['payload'] as String?;
          Map<String, dynamic>? payload;
          if (payloadJson != null && payloadJson.isNotEmpty) {
            try {
              payload = (jsonDecode(payloadJson) as Map<String, dynamic>)
                  .cast();
            } on Object {
              payload =
                  null; // Corrupt payload shouldn't hide the rest of the row.
            }
          }
          final deadLetteredAtMs = r['dead_lettered_at'] as num?;
          return DeadLetter(
            id: (r['id'] as num?)?.toInt() ?? 0,
            table: (r['table_name'] ?? '').toString(),
            op: (r['op'] ?? '').toString(),
            pk: (r['pk'] ?? '').toString(),
            attempts: (r['attempts'] as num?)?.toInt() ?? 0,
            payload: payload,
            error: r['last_error'] as String?,
            timestamp: deadLetteredAtMs == null
                ? null
                : DateTime.fromMillisecondsSinceEpoch(deadLetteredAtMs.toInt()),
          );
        })
        .toList(growable: false);
  }

  // ─────────────────── Reactive facade (ADR-0024) ───────────────────
  //
  // The DEFAULT beautiful dev surface: typed `Collection<T>` handles over the
  // existing hot-replay-shared watch pump, a derived `count`, typed collapsed
  // writes, and a hot `SyncStatus`. Raw SQL ([watch]/[getAll]) stays as the
  // escape hatch. See ADR-0024 + CONTEXT.md.

  /// A typed handle to one synced [table] — the DEFAULT dev surface.
  ///
  /// [fromRow] decodes a row `Map` into `T` (required). [toRow] encodes `T` for
  /// writes — **optional**; pass it only if you use [Collection.upsert]
  /// (read-only collections omit it). [pkColumn] names the primary-key column
  /// `toRow` emits (default `'id'`).
  /// ```dart
  /// final todos = db.collection<Todo>(
  ///   table: 'todos', fromRow: Todo.fromRow, toRow: (t) => t.toRow());
  /// final active = todos.watch(where: 'completed = 0'); // Stream<List<Todo>>
  /// await todos.upsert(Todo(id: '1', title: 'ship', completed: false));
  /// ```
  Collection<T> collection<T>({
    required String table,
    required T Function(Map<String, dynamic> row) fromRow,
    Map<String, dynamic> Function(T value)? toRow,
    String pkColumn = 'id',
  }) => Collection<T>._(this, table, fromRow, toRow, pkColumn);

  /// Hot sync status: connection state ([SyncStatus.conn],
  /// [SyncStatus.connected], [SyncStatus.lastSyncedAt]) folded together with
  /// the durable outbox ([SyncStatus.pendingWrites],
  /// [SyncStatus.lastWriteError] — ADR-0027).
  ///
  /// Still deferred: a download-progress / reconcile signal and `DataTrust`,
  /// which need engine-side signals that don't exist yet (ADR-0024).
  /// [SyncStatus.lastSyncedAt] remains a proxy stamped on each `connected`
  /// transition; the write half is now exact.
  ValueListenable<SyncStatus> get status {
    _ensureStatusWired();
    return _status!;
  }

  /// Synchronous snapshot of the current [SyncStatus].
  SyncStatus get currentStatus {
    _ensureStatusWired();
    return _status!.value;
  }

  ValueNotifier<SyncStatus>? _status;
  StreamSubscription<NostosConnectionState>? _statusSub;
  StreamSubscription<({int pending, int deadLettered, String? lastError})>?
  _writeStatusSub;
  StreamSubscription<bool>? _storageDegradedSub;
  bool _statusWired = false;
  bool _hasSubscribed = false;

  void _ensureStatusWired() {
    if (_statusWired) return;
    _statusWired = true;
    _status = ValueNotifier<SyncStatus>(
      const SyncStatus(
        conn: NostosConnectionState.disconnected,
        lastSyncedAt: null,
      ),
    );
    // ponytail: there is still no "download completed" / "reconcile done"
    // signal, so lastSyncedAt stays a best-effort proxy stamped on each
    // `connected` transition. The WRITE side is no longer a proxy — it comes
    // from the engine's real outbox (see the second subscription below).
    _statusSub = _nostos.connectionState.listen((s) {
      final prev = _status!.value;
      final lastSynced = s == NostosConnectionState.connected
          ? DateTime.now()
          : prev.lastSyncedAt;
      _status!.value = SyncStatus(
        conn: s,
        lastSyncedAt: lastSynced,
        pendingWrites: prev.pendingWrites,
        deadLetteredWrites: prev.deadLetteredWrites,
        lastWriteError: prev.lastWriteError,
        webStorageDegraded: prev.webStorageDegraded,
      );
    });
    // The other half of the later-of rule (see [_wireWriteStatus]): status
    // first read AFTER a subscribe → attach the pump now. (`_statusWired` is
    // already true above, so the recursive _ensureStatusWired call inside is
    // a no-op, not a loop.)
    if (_hasSubscribed) _wireWriteStatus();
  }

  /// Attach the outbox pump — at the LATER of first [status] access and first
  /// [subscribe], never eagerly. Two independent reasons, both load-bearing:
  ///
  /// 1. Precondition: the engine's `watchWriteStatus()` errors without an
  ///    active subscription, while the connection-state stream doesn't.
  ///    Reading [status] before subscribing is legitimate (you get the honest
  ///    `disconnected` default), so attaching at status-access time would turn
  ///    a valid call into a stream error.
  /// 2. Cost: apps that never read [status] never pay for the FFI stream.
  ///    This matters under high event rates — the zero-setup fake-replicator
  ///    server emits events unthrottled forever, and profiling showed any
  ///    session there saturates on the (pre-existing) full-snapshot watch
  ///    pumps within seconds; the SDK's own read-only e2e survives precisely
  ///    because it attaches nothing it doesn't use.
  ///
  /// Re-subscribing re-attaches: the old pump belongs to the replaced session,
  /// so it is cancelled rather than left orphaned.
  void _wireWriteStatus() {
    _ensureStatusWired();
    unawaited(_writeStatusSub?.cancel());
    // Two streams, one ValueListenable: the connection and the outbox change
    // independently (a write queues while offline; a dead-letter arrives while
    // connected), so each listener carries the other's fields forward rather
    // than resetting them.
    _writeStatusSub = _nostos.writeStatus.listen(
      (w) {
        final prev = _status!.value;
        _status!.value = SyncStatus(
          conn: prev.conn,
          lastSyncedAt: prev.lastSyncedAt,
          pendingWrites: w.pending,
          deadLetteredWrites: w.deadLettered,
          lastWriteError: w.lastError,
          webStorageDegraded: prev.webStorageDegraded,
        );
      },
      // A dead pump must not take the app with it: the connection half of
      // SyncStatus keeps working, and the write counts simply stop updating.
      onError: (Object _) {},
    );
    // ADR-0036: fold the web storage-degrade signal in. Native never fires
    // (empty stream → this listener stays idle), so this is a no-op there.
    unawaited(_storageDegradedSub?.cancel());
    _storageDegradedSub = _nostos.webStorageDegraded.listen((degraded) {
      final prev = _status!.value;
      if (prev.webStorageDegraded == degraded) return;
      _status!.value = SyncStatus(
        conn: prev.conn,
        lastSyncedAt: prev.lastSyncedAt,
        pendingWrites: prev.pendingWrites,
        deadLetteredWrites: prev.deadLetteredWrites,
        lastWriteError: prev.lastWriteError,
        webStorageDegraded: degraded,
      );
    }, onError: (Object _) {});
  }

  /// Tear down the underlying [Nostos] session (sync loop + watch pump) AND the
  /// status listener. Safe to call with no subscription; idempotent.
  Future<void> close() async {
    // Auth listener first: it calls into the engine, so leaving it attached
    // across the close below would let a token refresh hit a closed engine.
    await _authSub?.cancel();
    await _statusSub?.cancel();
    await _writeStatusSub?.cancel();
    await _storageDegradedSub?.cancel();
    _status?.dispose();
    await _nostos.close();
  }

  /// Register a hook wiped on [signOut] (ADR-0029). The T6 attachments driver
  /// uses this to wipe local blobs so the next principal sees none of the
  /// prior user's bytes. Hooks are awaited AFTER the core wipe, in order, and
  /// MUST be idempotent. Returns without awaiting the hook.
  void registerSignOutHook(Future<void> Function() hook) =>
      _signOutHooks.add(hook);

  /// ADR-0029: sign out — wipe the local store + durable outbox (the next
  /// principal sees nothing of this one), stop sync, clear the seed token, and
  /// cancel the auth/status listeners. Unlike [close], the on-device SQLite
  /// state is wiped via `clear_local_state`. Idempotent.
  ///
  /// T6 (ADR-0034): registered [_signOutHooks] (e.g. the blob-store wipe) run
  /// AFTER the core wipe so the engine is already quiesced — a hook never sees
  /// in-flight apply frames.
  ///
  /// Ordering (L1): the seed token is cleared AFTER the hooks — the
  /// push-token deregistration hook authorizes its DELETEs with it. In
  /// Supabase apps call THIS signOut BEFORE `supabase.auth.signOut()`:
  /// the hook reads the live Supabase session (`_restAuthToken`), which 401s
  /// once Supabase has already signed out.
  Future<void> signOut() async {
    // Auth listener first (as in close): a surviving refresh would call
    // setToken on a wiped engine.
    await _authSub?.cancel();
    await _statusSub?.cancel();
    await _writeStatusSub?.cancel();
    await _storageDegradedSub?.cancel();
    _status?.dispose();
    await _nostos.signOut();
    // Wipe extra local surfaces (blobs) AFTER the engine is quiesced + wiped.
    // Best-effort: a failing hook is logged-and-swallowed so it cannot block
    // the (already-complete) core sign-out. Run a snapshot so a re-entrant
    // register during wipe cannot mutate the list under us.
    for (final hook in List<Future<void> Function()>.of(_signOutHooks)) {
      try {
        await hook();
      } on Object {
        // Swallowed deliberately — see method doc.
      }
    }
    // L1: the REST credential dies with the session — a post-signOut
    // push-token call is anonymous, never the previous principal's.
    _seedToken = null;
  }

  /// Pause syncing (ADR-0032 T1 canonical name): abort ONLY the background
  /// connect loop, keeping the client, its on-device SQLite store, the token,
  /// the schema, and every `watch()` pump alive. Reads, writes (enqueued to the
  /// durable outbox), and the UI keep working offline. Emits `disconnected` on
  /// [connectionState]. Idempotent.
  ///
  /// [resumeSync] restarts the connect loop on the same client — the outbox
  /// flushes on reconnect and live updates resume, and watches re-emit their
  /// latest value without the caller re-wiring anything (the pumps are
  /// hot-replay-shared and survive a pause). No wire-protocol change.
  Future<void> pauseSync() => _nostos.disconnect();

  /// Resume syncing after [pauseSync] (ADR-0032 T1 canonical name). Restarts
  /// the connect loop on the same client; the durable outbox drains on
  /// reconnect and live updates resume. Emits `connecting → connected …` on
  /// [connectionState]. Requires a prior [subscribe] (throws otherwise).
  ///
  /// Throws [StateError] on a [NostosDatabase.local] database — there is no
  /// sync to resume (the placeholder URL would flap forever). Reopen the same
  /// SQLite file with a real `/sync` URL to start syncing.
  void resumeSync() {
    if (_localOnly) {
      throw StateError(
        'resumeSync() on a NostosDatabase.local database: there is no sync to '
        'resume. To start syncing, reopen the same SQLite file via '
        'NostosDatabase.connect/open/supabase with a real /sync URL.',
      );
    }
    _nostos.resume();
  }

  /// Legacy alias of [pauseSync]. Prefer `pauseSync()` (ADR-0032); this name is
  /// kept for back-compat with code written against the pre-contract surface.
  Future<void> disconnect() => pauseSync();

  /// Legacy alias of [resumeSync]. Prefer `resumeSync()` (ADR-0032).
  void resume() => resumeSync();

  // ─────────────────── Push tokens (ADR-0037 §3) ───────────────────
  //
  // Doorbell semantics: push is a hint, sync is the transport. Registering a
  // token only tells the server WHERE to knock; the data always arrives over
  // the sync connection, which resumes from the durable LSN checkpoint
  // ([pauseSync]/[resumeSync], or a cold `connect` + `subscribe` from a
  // killed app). Row data never transits Apple/Google servers.

  /// Push tokens registered via [registerPushToken] THIS session,
  /// deregistered by the [signOut] hook (ADR-0037 §3 — a leaked registration
  /// would push the previous principal's data to the next user).
  ///
  /// ponytail: in-memory only — tokens registered before a process restart
  /// are not auto-deregistered (the set dies with the process). The stale
  /// case is covered server-side: the rails prune on APNs 410 / FCM
  /// `UNREGISTERED`. Upgrade path: persist the set in the local store if
  /// rail-prune proves too slow for real tenants.
  final Set<String> _registeredPushTokens = <String>{};

  /// The platforms `POST /push-tokens` accepts (ADR-0037 §3).
  static const Set<String> _pushPlatforms = {'fcm', 'apns', 'webpush'};

  /// Register this device's push token with the server (ADR-0037 §3):
  /// `POST /push-tokens` with `{"platform": …, "token": …}`, authenticated by
  /// the SAME JWT the sync connection uses (`Authorization: Bearer`). The
  /// server stamps tenant/account itself — the SDK never attests identity
  /// fields.
  ///
  /// [platform] is one of `"fcm"`, `"apns"`, `"webpush"`. Call this whenever
  /// the OS push service hands the app a (possibly rotated) token — e.g.
  /// `FirebaseMessaging.onTokenRefresh` on Android, APNs
  /// `didRegisterForRemoteNotificationsWithDeviceToken` on iOS.
  ///
  /// Throws [ArgumentError] for an unknown platform, or
  /// [NostosPushTokenException] when the server replies anything other than
  /// `204`. Registered tokens are deregistered automatically by [signOut].
  Future<void> registerPushToken(String platform, String token) async {
    if (_localOnly) {
      throw StateError(
        'registerPushToken() on a NostosDatabase.local database: there is no '
        'server to register with. Push requires a sync URL — reopen via '
        'NostosDatabase.connect/open/supabase.',
      );
    }
    if (!_pushPlatforms.contains(platform)) {
      throw ArgumentError.value(
        platform,
        'platform',
        'must be one of ${_pushPlatforms.toList()}',
      );
    }
    if (token.isEmpty) {
      throw ArgumentError.value(token, 'token', 'must be non-empty');
    }
    await _pushTokensRest(
      'POST',
      '/push-tokens',
      body: jsonEncode(<String, String>{'platform': platform, 'token': token}),
    );
    _registeredPushTokens.add(token);
  }

  /// Deregister [token] (ADR-0037 §3): `DELETE /push-tokens/{token}` with the
  /// same auth as [registerPushToken]. Call this when the app can no longer
  /// receive on this token (e.g. the user disables notifications);
  /// [signOut] deregisters every session-registered token automatically.
  ///
  /// Throws [NostosPushTokenException] when the server replies anything other
  /// than `204`.
  Future<void> deregisterPushToken(String token) async {
    if (_localOnly) {
      throw StateError(
        'deregisterPushToken() on a NostosDatabase.local database: there is no '
        'server to deregister from.',
      );
    }
    await _pushTokensRest(
      'DELETE',
      '/push-tokens/${Uri.encodeComponent(token)}',
    );
    _registeredPushTokens.remove(token);
  }

  /// One REST round-trip against the pinned push-token contract. Sends the
  /// JSON [body] for `POST` (none for `DELETE`) and the sync JWT as a Bearer
  /// header when one exists (a `NOSTOS_SYNC_AUTH=none` server has no token to
  /// send, same as the WS handshake).
  ///
  /// A 401 triggers ONE credential re-acquisition ([_sessionRefresh]) and a
  /// single retry — a cold-restored Supabase session can serve an expired
  /// access token before the auto-refresh timer rotates it, and push-token
  /// registration must not silently strand the device (observed in the
  /// ADR-0037 pilot: emulator registered nothing, pushes went only to the
  /// phone). The refreshed token is re-read via [_restAuthToken], never
  /// threaded through by hand.
  Future<void> _pushTokensRest(
    String method,
    String path, {
    String? body,
  }) async {
    var response = await _restRoundTrip(method, path, body: body);
    if (response.statusCode == 401) {
      final refresh = _sessionRefresh;
      final fresh = refresh == null ? null : await refresh();
      if (fresh != null) {
        if (!_supabaseAuth) _seedToken = fresh; // adopt on the seed path
        response = await _restRoundTrip(method, path, body: body);
      }
    }
    if (response.statusCode != 204) {
      throw NostosPushTokenException(
        operation: method == 'POST' ? 'register' : 'deregister',
        statusCode: response.statusCode,
        body: response.body,
      );
    }
  }

  Future<http.Response> _restRoundTrip(
    String method,
    String path, {
    String? body,
  }) {
    final token = _restAuthToken;
    final uri = Uri.parse('$_httpBase$path');
    final headers = <String, String>{
      if (body != null) 'content-type': 'application/json',
      if (token != null) 'authorization': 'Bearer $token',
    };
    return _retryConn(
      () => method == 'POST'
          ? http.post(uri, headers: headers, body: body)
          : http.delete(uri, headers: headers),
    );
  }

  /// Sign-out hook: best-effort DELETE of every session-registered token.
  /// Per-token failures are swallowed (one failed DELETE must not block the
  /// rest — the stale row is pruned server-side, see the
  /// [_registeredPushTokens] ponytail). Idempotent: [deregisterPushToken]
  /// removes each token from the set as it succeeds.
  Future<void> _deregisterPushTokensOnSignOut() async {
    for (final token in List<String>.of(_registeredPushTokens)) {
      try {
        await deregisterPushToken(token);
      } on Object {
        // Swallowed — see method doc.
      }
    }
  }

  /// Awaitable barrier that completes once the first sync has landed, i.e. the
  /// session has reached `connected` at least once (ADR-0032 T1). Resolves
  /// immediately if sync has already happened (so it is safe to call on every
  /// reconnect / app start). Use this instead of polling [SyncStatus.hasSynced].
  ///
  /// ponytail: `lastSyncedAt` is a proxy stamped on each `connected`
  /// transition, not a precise "download completed / reconcile done" signal
  /// (which the engine does not yet expose — ADR-0024). It is the same proxy
  /// [SyncStatus.hasSynced] uses; upgrading one upgrades the other.
  ///
  /// On a [NostosDatabase.local] database this resolves immediately — there is
  /// no server, so there is no first sync to wait for; the local row set is
  /// already the whole truth.
  Future<void> waitForFirstSync() {
    if (_localOnly) return Future<void>.value();
    if (_status?.value.lastSyncedAt != null) return Future<void>.value();
    _ensureStatusWired();
    final completer = Completer<void>();
    void listener() {
      if (_status!.value.lastSyncedAt != null && !completer.isCompleted) {
        completer.complete();
        _status!.removeListener(listener);
      }
    }

    _status!.addListener(listener);
    // If the very first connect already landed between the check above and
    // addListener, resolve now rather than deadlock.
    if (_status!.value.lastSyncedAt != null && !completer.isCompleted) {
      completer.complete();
      _status!.removeListener(listener);
    }
    return completer.future;
  }

  /// Derive the HTTP base for `GET /schema` from the WS `/sync` URL:
  /// `wss`→`https`, `ws`→`http`, host+port preserved, trailing path stripped.
  static String _deriveHttpBase(String wsUrl) {
    final uri = Uri.parse(wsUrl);
    final scheme = switch (uri.scheme) {
      'wss' => 'https',
      'ws' => 'http',
      _ => uri.scheme,
    };
    final port = uri.port == 0 ? '' : ':${uri.port}';
    return '$scheme://${uri.host}$port';
  }

  static Future<NostosSchema> _fetchSchema(String httpBase) async {
    final response = await _retryConn(
      () => http.get(Uri.parse('$httpBase/schema')),
    );
    final body = jsonDecode(response.body) as Map<String, dynamic>;
    return NostosSchema.fromSchemaDescriptor(body);
  }

  /// iOS 14+ gates first-time LAN access behind the Local Network permission:
  /// the triggering request fails (host-unreachable class errors) while the OS
  /// prompt is on screen, then succeeds once granted. Retry connection-level
  /// failures a few times before surfacing them; anything HTTP-level
  /// (status codes) never lands here. Registering a push token is idempotent
  /// server-side, so replaying a lost POST is safe.
  static Future<T> _retryConn<T>(Future<T> Function() fn) async {
    const attempts = 10;
    for (var i = 1;; i++) {
      try {
        return await fn();
      } on http.ClientException {
        if (i >= attempts) rethrow;
        await Future<void>.delayed(const Duration(seconds: 3));
      }
    }
  }
}

/// SQLite view name for a synced table (ADR-0028): the client engine
/// creates one view per synced table, but SQLite has no schema-qualified
/// local names, so a server-side `myschema.tasks` arrives as the view
/// `myschema_tasks` — the Rust `view_name()` rule, dots collapse to
/// underscores. Every structured query path normalizes through here so a
/// [Collection] over a non-public-schema table hits its view instead of
/// failing with "no such table: myschema.tasks". Raw-SQL callers
/// ([NostosDatabase.watch]/[NostosDatabase.getAll]) get no normalization —
/// they must write the collapsed view name themselves.
String _viewName(String table) => table.replaceAll('.', '_');

/// Compose a `SELECT * FROM <table>` SQL string from the structured
/// [where]/[orderBy]/[limit]/[offset] (ADR-0032 T2). The table name is
/// normalized through [_viewName] (schema-qualified names collapse to the
/// engine's view name). All inputs are injection-safe: column names are
/// identifier-validated in [Where.toSql]/[Order.toSql], and values are
/// emitted as literals (see `predicate.dart`).
String _composeQuery(
  String table, {
  Where? where,
  List<Order>? orderBy,
  int? limit,
  int? offset,
}) {
  var sql = 'SELECT * FROM ${_viewName(table)}';
  final w = where?.toSql();
  if (w != null) sql += ' WHERE $w';
  if (orderBy != null && orderBy.isNotEmpty) {
    sql += ' ORDER BY ${orderBy.map((o) => o.toSql()).join(', ')}';
  }
  if (limit != null) {
    if (limit < 0) {
      throw ArgumentError.value(limit, 'limit', 'must be >= 0');
    }
    sql += ' LIMIT $limit';
  }
  if (offset != null) {
    if (offset < 0) {
      throw ArgumentError.value(offset, 'offset', 'must be >= 0');
    }
    // SQLite requires LIMIT to be present when OFFSET is; use -1 (no limit) if
    // the caller wanted offset-only.
    if (limit == null) sql += ' LIMIT -1';
    sql += ' OFFSET $offset';
  }
  return sql;
}

/// Typed handle to one synced table — the beautiful default dev surface
/// (ADR-0024). Obtained via [NostosDatabase.collection].
///
/// - [watch] returns a typed `Stream<List<T>>` backed by the existing per-table
///   hot-replay-shared pump ([Nostos.watch]); multiple [watch] callers share the
///   upstream. `ValueListenableBuilder` users can adapt with a `Stream`→
///   `ValueNotifier` bridge (P1 helper; until then `StreamBuilder` is the path).
/// - [count] is a derived selector — a count widget does NOT rebuild on
///   unrelated column writes.
/// - [upsert]/[delete] are typed collapsed writes (the moat — no `uploadData`
///   toll-booth; ADR-0013).
class Collection<T> {
  Collection._(this._db, this.table, this._fromRow, this._toRow, this.pkColumn);

  final NostosDatabase _db;
  final String table;
  final T Function(Map<String, dynamic> row) _fromRow;
  final Map<String, dynamic> Function(T value)? _toRow;
  final String pkColumn;

  /// Reactive typed read (ADR-0032 T2). Re-runs whenever the table's synced
  /// data changes and re-emits the typed rows matching [where].
  ///
  /// - [where] is structured data, not a SQL fragment — build it with
  ///   [Where.eq]/[Where.gt]/[Where.and]/… (see `predicate.dart`). Column names
  ///   are identifier-validated and values are emitted as safe SQLite literals,
  ///   so nothing the caller supplies is spliced raw. This replaces the old
  ///   string-`where` and kills the injection foot-gun.
  /// - [orderBy] is a list of [Order] terms (first entry sorts first).
  /// - [limit]/[offset] map to SQL `LIMIT`/`OFFSET`.
  /// - [throttle] coalesces a burst of change ticks into one re-query per
  ///   window.
  Stream<List<T>> watch({
    Where? where,
    List<Order>? orderBy,
    int? limit,
    int? offset,
    Duration? throttle,
  }) {
    final sql = _composeQuery(
      table,
      where: where,
      orderBy: orderBy,
      limit: limit,
      offset: offset,
    );
    return _db
        .watch(sql, throttle: throttle)
        .map((rows) => rows.map(_fromRow).toList(growable: false));
  }

  /// One-shot typed read (ADR-0032 T2): the non-reactive twin of [watch].
  /// Same [where]/[orderBy]/[limit]/[offset] semantics, run once.
  Future<List<T>> getAll({
    Where? where,
    List<Order>? orderBy,
    int? limit,
    int? offset,
  }) async {
    final sql = _composeQuery(
      table,
      where: where,
      orderBy: orderBy,
      limit: limit,
      offset: offset,
    );
    final rows = await _db.getAll(sql);
    return rows.map(_fromRow).toList(growable: false);
  }

  /// One-shot single-row fetch by primary key (ADR-0032 T2 — `fetchById`
  /// parity). Returns `null` if no row matches. `fetchById` is an alias.
  Future<T?> get(Object pk) => _get(pk);

  /// Alias of [get] (the name sibling SDKs expose). Kept for cross-SDK parity.
  Future<T?> fetchById(Object pk) => _get(pk);

  Future<T?> _get(Object pk) async {
    final rows = await getAll(where: Where.eq(pkColumn, pk), limit: 1);
    return rows.isEmpty ? null : rows.first;
  }

  /// Reactive single-row watch by primary key (ADR-0032 T2 — WatermelonDB
  /// `findAndObserve` parity). Emits the row matching [pk], or `null` when it's
  /// absent; re-emits on any change to that row. Detail screens use this so they
  /// don't rebuild on unrelated list churn.
  Stream<T?> watchOne(Object pk) {
    final sql = _composeQuery(table, where: Where.eq(pkColumn, pk), limit: 1);
    return _db.watch(sql).map((rows) {
      if (rows.isEmpty) return null;
      return _fromRow(rows.first);
    });
  }

  /// Derived count — emits the row count matching [where], re-runs on table
  /// change (ADR-0032 T2). Use this for count badges so they don't rebuild on
  /// unrelated column writes.
  Stream<int> count({Where? where}) {
    final whereFragment = where?.toSql();
    final view = _viewName(table);
    final sql = whereFragment == null
        ? 'SELECT COUNT(*) AS count FROM $view'
        : 'SELECT COUNT(*) AS count FROM $view WHERE $whereFragment';
    return _db.watch(sql).map((rows) {
      final v = rows.isEmpty ? null : rows.first['count'];
      return v is num ? v.toInt() : 0;
    });
  }

  /// Reactive boolean — emits whether any row matches [where], re-runs on
  /// table change (ADR-0032 T2). Cheaper to render than [count] for "is there
  /// any…" UI (empty-vs-nonempty badges, conditional affordances).
  Stream<bool> exists({Where? where}) {
    final whereFragment = where?.toSql();
    final view = _viewName(table);
    final sql = whereFragment == null
        ? 'SELECT EXISTS(SELECT 1 FROM $view) AS hit'
        : 'SELECT EXISTS(SELECT 1 FROM $view WHERE $whereFragment) AS hit';
    return _db.watch(sql).map((rows) {
      final v = rows.isEmpty ? null : rows.first['hit'];
      return v is num ? v.toInt() != 0 : false;
    });
  }

  /// Typed collapsed write: encodes [value] via `toRow` and enqueues an upsert
  /// into the durable outbox. Returns the local outbox id (NOT a server ack);
  /// the applied row round-trips back through [watch] (ADR-0013).
  ///
  /// Throws [StateError] if no `toRow` was provided to `collection<T>()`, or
  /// [ArgumentError] if `toRow(value)` omits the [pkColumn] column.
  Future<int> upsert(T value) {
    if (_toRow == null) {
      throw StateError(
        'Collection($table).upsert: no toRow was provided to collection<T>(). '
        'Pass toRow when constructing the collection to use typed writes.',
      );
    }
    final row = _toRow(value);
    final pk = row[pkColumn]?.toString();
    if (pk == null) {
      throw ArgumentError(
        'Collection($table).upsert: toRow() returned no "$pkColumn" column.',
      );
    }
    return _db.write(table: table, op: 'upsert', pk: pk, payload: row);
  }

  /// Map-based full-row upsert for form-driven writes. A form dialog returns a
  /// `Map<String,String>`; constructing a typed `T` only to re-encode it would
  /// be circular here because the read-model is a *projection* (a subset of
  /// columns with parsed types), not a full write-image — e.g. a write payload
  /// stamps `created_at` server-side, a field the read-model lacks. [row] must
  /// include the [pkColumn]. Prefer [upsert] (typed) when you have a full `T`
  /// with a `toRow`.
  Future<int> upsertRow(Map<String, dynamic> row) {
    final pk = row[pkColumn]?.toString();
    if (pk == null) {
      throw ArgumentError(
        'Collection($table).upsertRow: row omits the "$pkColumn" column.',
      );
    }
    return _db.write(table: table, op: 'upsert', pk: pk, payload: row);
  }

  /// Column-level patch: update only [columns] of the row identified by [pk].
  /// The row is never inserted; columns absent from [columns] are untouched.
  /// This is the canonical partial-update path — server-authoritative per-field
  /// LWW (ADR-0014). Use it for status flips and single-field edits.
  Future<int> patch(Object pk, Map<String, dynamic> columns) =>
      _db.write(table: table, op: 'patch', pk: pk.toString(), payload: columns);

  /// Delete the row whose primary key is [pk].
  Future<int> delete(Object pk) =>
      _db.write(table: table, op: 'delete', pk: pk.toString());

  // ─────────────────── CRDT handles (ADR-0030 / ADR-0032 T4) ───────────────────
  //
  // OR-set (add-wins) handles for columns the server tags as an OR-set. Unlike
  // [upsert]/[patch] (per-field last-writer-wins, ADR-0014), an OR-set column
  // MERGES: concurrent adds of different elements both survive, and a remove is
  // a tombstone that a concurrent or later re-add revives (add-wins). Use these
  // for multi-value fields (tags, collaborators, reactions) where LWW would
  // clobber concurrent additions.
  //
  // Counters (PN-Counter, ADR-0030 addendum) merge per-replica: each client
  // owns its positive/negative entry, and `apply_local` takes the elementwise
  // max across replicas on the server's echo — no clobbering. Use these for
  // tallies (likes, views, scores) where LWW would lose concurrent increments.
  //
  // Both families REQUIRE the table to be declared as a CRDT table at open:
  // pass [NostosDatabase.connect]/[NostosDatabase.open]/[NostosDatabase.supabase]
  // an `orSetTables` / `counterTables` set. Without it the verb throws
  // `*TableNotTagged` (the gate) and writes clobber instead of merge. The
  // declared set MUST also match the server's `NOSTOS_OR_SET_COLUMNS` /
  // `NOSTOS_COUNTER_COLUMNS`, or client-merge and server-clobber disagree.

  /// Add [element] to the OR-set column in row [pk] of this table (ADR-0030 /
  /// ADR-0032 T4). Mints a client HLC and enqueues a merge-upsert; the element
  /// renders locally immediately and converges with concurrent remote adds on
  /// the server's echo. Returns the local outbox id.
  ///
  /// Requires the table to be declared in the `orSetTables` set passed to
  /// [NostosDatabase.connect]/[NostosDatabase.open]/[NostosDatabase.supabase] (and
  /// the server's `NOSTOS_OR_SET_COLUMNS`) — without it the verb throws
  /// `OrSetTableNotTagged`.
  Future<int> orSetAdd({required Object pk, required String element}) =>
      _db._nostos.orSetAdd(table: table, pk: pk.toString(), element: element);

  /// Remove [element] from the OR-set column in row [pk] — a tombstone at a
  /// fresh HLC. Add-wins: a concurrent or later re-add revives the element.
  /// Returns the local outbox id.
  Future<int> orSetRemove({required Object pk, required String element}) =>
      _db._nostos.orSetRemove(table: table, pk: pk.toString(), element: element);

  /// Increment the PN-Counter in row [pk] of this table by [delta] (ADR-0030
  /// addendum). Read-modify-write: reads the current counter payload, applies
  /// the delta to this replica's entry, and enqueues a merge-upsert. Converges
  /// with concurrent remote increments on the server's echo. Returns the local
  /// outbox id.
  ///
  /// Requires the table to be declared in the `counterTables` set passed to
  /// [NostosDatabase.connect]/[NostosDatabase.open]/[NostosDatabase.supabase] (and
  /// the server's `NOSTOS_COUNTER_COLUMNS`) — without it the verb throws
  /// `CounterTableNotTagged`.
  Future<int> counterIncrement({required Object pk, required int delta}) => _db
      ._nostos
      .counterIncrement(table: table, pk: pk.toString(), delta: delta);

  /// Decrement the PN-Counter by [delta] (bumps the negative counter `n` for
  /// this replica). Returns the local outbox id.
  Future<int> counterDecrement({required Object pk, required int delta}) => _db
      ._nostos
      .counterDecrement(table: table, pk: pk.toString(), delta: delta);

  /// Single-table [NostosDatabase.writeBatch] convenience (ADR-0032 T3): stamps
  /// this collection's [table] onto every op. Same all-or-nothing-delivery,
  /// NOT-a-server-transaction semantics — see [NostosDatabase.writeBatch].
  Future<List<int>> writeBatch(List<NostosWrite> writes) {
    final stamped = writes
        .map(
          (w) =>
              NostosWrite(table: table, op: w.op, pk: w.pk, payload: w.payload),
        )
        .toList(growable: false);
    return _db.writeBatch(stamped);
  }
}

/// Honest P0 sync status. Carries the connection state and the last time we
/// transitioned to `connected`.
///
/// Richer fields (`syncing`, `reconciling`, `uploadError`, `downloadError`) and
/// `DataTrust { fresh, stale, reconciling }` are **gated** — they land in P1
/// once (a) the engine exposes the signals and (b) the P0 sync fixes (client
/// WAL backfill across offline gaps; offline hard-delete orphan reconciliation)
/// ship, so `DataTrust` can be true instead of a permanent `stale` badge
/// (ADR-0024). Singleton on [NostosDatabase.status].
class SyncStatus {
  const SyncStatus({
    required this.conn,
    required this.lastSyncedAt,
    this.pendingWrites = 0,
    this.deadLetteredWrites = 0,
    this.lastWriteError,
    this.webStorageDegraded = false,
  });

  /// Writes captured locally but not yet ack'd by the server.
  ///
  /// `> 0` is normal and healthy while offline — that IS the offline-first
  /// promise. Show it as "N unsynced changes", not as an error.
  final int pendingWrites;

  /// Writes that permanently failed this session and were removed from the
  /// send queue. Unlike [pendingWrites], this number never goes down on its
  /// own: it counts data the user will lose unless the app does something.
  final int deadLetteredWrites;

  /// The server's message for the most recent permanent write failure, or
  /// `null` if none.
  ///
  /// Deliberately NOT set for ordinary rejections — those are frequently
  /// transient and retry on their own, so surfacing them would teach users to
  /// dismiss write errors. When this is non-null a write is genuinely lost and
  /// a human should be told. The text is the server's verbatim reason and is
  /// usually actionable (e.g. a `NOSTOS_WRITE_TABLES` rejection names the exact
  /// env var to set).
  final String? lastWriteError;

  /// Web-only (ADR-0036): true when the browser storage backend degraded to
  /// in-memory because OPFS was unavailable (Safari Private Browsing, old
  /// browsers, OPFS disallowed). Always false on native. When true, rows +
  /// outbox do NOT survive a reload — surface a "session not persisted"
  /// banner so the user knows to use a non-private window.
  final bool webStorageDegraded;

  /// True when at least one write is permanently lost. This is the condition
  /// Flutter's own optimistic-state guidance expects you to render (revert the
  /// optimistic value and tell the user) — before this existed, a Nostos app
  /// had no way to detect it.
  bool get hasWriteError => lastWriteError != null;

  /// True when there is local work the server hasn't confirmed yet.
  bool get hasPendingWrites => pendingWrites > 0;

  /// True while connected with queued writes still draining.
  bool get uploading => connected && pendingWrites > 0;

  /// True once a sync has completed at least once — use it to tell "empty
  /// because nothing synced yet" apart from "empty because there is no data".
  bool get hasSynced => lastSyncedAt != null;

  /// Current connection state of the underlying sync session.
  final NostosConnectionState conn;

  /// Last time the session transitioned to `connected` (null before the first
  /// successful connect). Best-effort proxy for "last synced" until the engine
  /// exposes a download-completed signal (P1).
  final DateTime? lastSyncedAt;

  /// Convenience: true when [conn] is [NostosConnectionState.connected].
  bool get connected => conn == NostosConnectionState.connected;

  @override
  String toString() =>
      'SyncStatus(conn: $conn, connected: $connected, lastSyncedAt: $lastSyncedAt, '
      'pendingWrites: $pendingWrites, deadLetteredWrites: $deadLetteredWrites, '
      'webStorageDegraded: $webStorageDegraded, '
      'lastWriteError: $lastWriteError)';
}

/// One write op inside a [NostosDatabase.writeBatch] group (ADR-0032 T3).
/// `op` is `"upsert"`, `"delete"`, or `"patch"` (same vocabulary as
/// [NostosDatabase.write]); `payload` is the row image (upsert) or column
/// subset (patch), and is `null` for deletes.
@immutable
class NostosWrite {
  const NostosWrite({
    required this.table,
    required this.op,
    required this.pk,
    this.payload,
  });

  final String table;
  final String op;
  final Object pk;
  final Map<String, dynamic>? payload;

  @override
  String toString() =>
      'NostosWrite(table: $table, op: $op, pk: $pk, payload: $payload)';
}

/// One permanently-failed write surfaced by [NostosDatabase.deadLetters]
/// (ADR-0032 T5 / ADR-0027). `error` is the server's verbatim per-row reason
/// and `timestamp` is when the flush loop quarantined it (both persisted since
/// the `last_error`/`dead_lettered_at` outbox migration). `attempts` is the
/// flush-retry count at the point the row was quarantined.
@immutable
class DeadLetter {
  const DeadLetter({
    required this.id,
    required this.table,
    required this.op,
    required this.pk,
    required this.attempts,
    this.payload,
    this.error,
    this.timestamp,
  });

  final int id;
  final String table;
  final String op;
  final String pk;
  final int attempts;
  final Map<String, dynamic>? payload;

  /// Server's per-row reason for the permanent failure (verbatim).
  final String? error;

  /// When the row was quarantined (epoch-ms → [DateTime]).
  final DateTime? timestamp;

  @override
  String toString() =>
      'DeadLetter(id: $id, table: $table, op: $op, pk: $pk, attempts: $attempts)';
}

/// A push-token REST call failed (ADR-0037 §3) — the server answered with
/// anything other than the pinned `204`. [statusCode] is the HTTP status
/// (e.g. `401` for an expired JWT) and [body] the verbatim response body.
@immutable
class NostosPushTokenException implements Exception {
  const NostosPushTokenException({
    required this.operation,
    required this.statusCode,
    required this.body,
  });

  /// Which call failed: `'register'` or `'deregister'`.
  final String operation;

  /// The server's HTTP status code.
  final int statusCode;

  /// The server's verbatim response body (may be empty).
  final String body;

  @override
  String toString() =>
      'NostosPushTokenException: push-token $operation failed with HTTP '
      '$statusCode: $body';
}
