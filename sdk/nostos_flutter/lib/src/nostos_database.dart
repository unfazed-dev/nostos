import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:supabase_flutter/supabase_flutter.dart';

import 'nostos.dart';
import 'nostos_config.dart';
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
  NostosDatabase._(this._nostos, this.schema);

  /// Test-only: wrap an injected [Nostos] (itself injectable via
  /// `Nostos.withEngine`) to exercise the typed mappers ([watchMapped] /
  /// [getAllMapped]) without the native library. See
  /// `test/nostos_ws6_test.dart`.
  @visibleForTesting
  NostosDatabase.forTest(this._nostos, this.schema);

  final Nostos _nostos;

  /// The resolved server schema used to materialize the read-views.
  /// Exposed for inspection / codegen; not meant to be mutated.
  final NostosSchema schema;

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
  }) =>
      _open(url: url, token: token, schema: schema, sqlitePath: sqlitePath);

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
  /// ponytail: the access token is read ONCE at connect time. Transparent
  /// refresh on token rotation via
  /// `Supabase.instance.client.auth.onAuthStateChange` (re-binding the
  /// token on `tokenRefreshed` / `initialSession` events) is a deliberate
  /// v1 fast-follow — until then, long-lived sessions that rotate the
  /// token mid-flight will eventually hit 401s and need a reconnect. The
  /// upgrade path is to subscribe to `onAuthStateChange` inside this
  /// factory and forward the new token to the underlying `Nostos` (see
  /// `NostosSupabase` for the token-swap primitive).
  static Future<NostosDatabase> supabase({
    required String nostosUrl,
    NostosSchema? schema,
    required String sqlitePath,
  }) async {
    final session = Supabase.instance.client.auth.currentSession;
    if (session == null) {
      throw StateError(
        'no Supabase session — sign in before calling NostosDatabase.supabase()',
      );
    }
    return _open(
      url: nostosUrl,
      token: session.accessToken,
      schema: schema,
      sqlitePath: sqlitePath,
    );
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
  }) async {
    final nostos = await Nostos.connect(
      url: url,
      token: token,
      sqlitePath: sqlitePath,
    );
    final resolved = schema ?? await _fetchSchema(_deriveHttpBase(url));
    nostos.applySchema(resolved.toClientTables());
    return NostosDatabase._(nostos, resolved);
  }

  /// Connection-state transitions for the underlying [Nostos] session.
  Stream<NostosConnectionState> get connectionState =>
      _nostos.connectionState;

  /// Subscribe to [table], optionally filtered by [where] (a safe-SQL
  /// predicate — see `Nostos.subscribe`). Must be called before [watch] /
  /// [getAll] / [write] for that table. For multiple tables on one
  /// connection, use [subscribeTables].
  Future<void> subscribe(String table, {String? where}) =>
      _nostos.subscribe(table, where: where);

  /// Subscribe to [tables] over one `/sync` socket (D1/ADR-0022 multi-table).
  /// Each entry may carry its own `whereSql`. Replaces any prior subscription.
  /// Call once with the full table set, then [watch] / [getAll] / [write] per
  /// table.
  Future<void> subscribeTables(List<NostosTableSub> tables) =>
      _nostos.subscribeTables(tables);

  /// Reactive SQL watch: re-runs [sql] whenever the synced data changes and
  /// emits the decoded result set. Thin delegate over `Nostos.watchQuery`
  /// (PowerSync-parity P1). Requires an active [subscribe] first.
  Stream<List<Map<String, dynamic>>> watch(String sql, {Duration? throttle}) =>
      _nostos.watchQuery(sql, throttle: throttle);

  /// Run a one-shot SELECT against on-device SQLite and return the decoded
  /// rows. Non-reactive counterpart to [watch]. Requires an active
  /// [subscribe] (the engine enforces this).
  Future<List<Map<String, dynamic>>> getAll(String sql) async =>
      (jsonDecode(await _nostos.query(sql)) as List<dynamic>)
          .cast<Map<String, dynamic>>();

  /// Raw-SQL execute. Currently a READ-ONLY alias of [getAll].
  ///
  /// ponytail: writes through raw SQL are a deliberate fast-follow ceiling
  /// — the demo's add/delete/edit flows all route through [write] (which
  /// enqueues into the durable outbox and round-trips the applied row back
  /// through [watch]). Accepting arbitrary INSERT/UPDATE/DELETE here would
  /// bypass the outbox and desync the local view from the server's
  /// replication stream. Parse raw SQL for writes in a follow-up and route
  /// them into [write]; until then, [execute] is SELECT-only.
  Future<List<Map<String, dynamic>>> execute(String sql) => getAll(sql);

  /// Reactive typed-record watch (WS6): like [watch] but maps each row to a
  /// typed record via [fromRow]. Thin delegate over `Nostos.watchMapped`.
  Stream<List<T>> watchMapped<T>(
    String sql,
    T Function(Map<String, dynamic> row) fromRow,
  ) =>
      _nostos.watchMapped(sql, fromRow);

  /// One-shot typed-record query (WS6): like [getAll] but maps each row to a
  /// typed record via [fromRow].
  Future<List<T>> getAllMapped<T>(
    String sql,
    T Function(Map<String, dynamic> row) fromRow,
  ) async =>
      (await getAll(sql)).map(fromRow).toList(growable: false);

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
  }) =>
      _nostos.write(table, op: op, pk: pk, payload: payload);

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
  }) =>
      Collection<T>._(this, table, fromRow, toRow, pkColumn);

  /// Hot sync status. Honest P0: carries [SyncStatus.conn] (from the
  /// underlying connection stream) + [SyncStatus.connected] +
  /// [SyncStatus.lastSyncedAt]. Richer fields (syncing/reconciling/errors) and
  /// `DataTrust` land in P1 once the engine exposes those signals (ADR-0024).
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
  bool _statusWired = false;

  void _ensureStatusWired() {
    if (_statusWired) return;
    _statusWired = true;
    _status = ValueNotifier<SyncStatus>(const SyncStatus(
      conn: NostosConnectionState.disconnected,
      lastSyncedAt: null,
    ));
    // ponytail: the engine exposes only NostosConnectionState today. There is no
    // "download completed" / "reconcile done" / error signal yet, so lastSyncedAt
    // is stamped on each `connected` transition (a best-effort proxy) and the
    // richer SyncStatus fields are deferred to P1 with engine-side signals.
    _statusSub = _nostos.connectionState.listen((s) {
      final prev = _status!.value;
      final lastSynced = s == NostosConnectionState.connected
          ? DateTime.now()
          : prev.lastSyncedAt;
      _status!.value = SyncStatus(conn: s, lastSyncedAt: lastSynced);
    });
  }

  /// Tear down the underlying [Nostos] session (sync loop + watch pump) AND the
  /// status listener. Safe to call with no subscription; idempotent.
  Future<void> close() async {
    await _statusSub?.cancel();
    _status?.dispose();
    await _nostos.close();
  }

  /// Pause syncing (delegate to [Nostos.disconnect]); reads/writes/UI keep
  /// working offline. See `Nostos.disconnect`.
  Future<void> disconnect() => _nostos.disconnect();

  /// Resume syncing after [disconnect] (delegate to [Nostos.resume]).
  void resume() => _nostos.resume();

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
    final response = await http.get(Uri.parse('$httpBase/schema'));
    final body = jsonDecode(response.body) as Map<String, dynamic>;
    return NostosSchema.fromSchemaDescriptor(body);
  }
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

  /// Reactive typed read. Re-runs whenever the table's synced data changes.
  ///
  /// - [where] is a literal SQL fragment (e.g. `'completed = 0'`). Parameter
  ///   binding (`parameters: [...]`) is P1 — the engine query path is
  ///   parameter-less today; until then pass constants, **never** interpolated
  ///   user input.
  /// - [orderBy] is a literal `ORDER BY` fragment (e.g. `'starts_at'` or
  ///   `'created_at DESC'`), appended after [where]. Prefer this to stuffing
  ///   `ORDER BY` into [where].
  /// - [throttle] coalesces a burst of change ticks into one re-query per
  ///   window.
  Stream<List<T>> watch({
    String? where,
    Duration? throttle,
    String? orderBy,
  }) {
    var sql = 'SELECT * FROM $table';
    if (where != null) sql += ' WHERE $where';
    if (orderBy != null) sql += ' ORDER BY $orderBy';
    return _db
        .watch(sql, throttle: throttle)
        .map((rows) => rows.map(_fromRow).toList(growable: false));
  }

  /// Derived count — emits the row count matching [where], re-runs on table
  /// change. Use this for count badges so they don't rebuild on unrelated
  /// column writes.
  Stream<int> count({String? where}) {
    final sql = where == null
        ? 'SELECT COUNT(*) AS count FROM $table'
        : 'SELECT COUNT(*) AS count FROM $table WHERE $where';
    return _db.watch(sql).map((rows) {
      final v = rows.isEmpty ? null : rows.first['count'];
      return v is num ? v.toInt() : 0;
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
  const SyncStatus({required this.conn, required this.lastSyncedAt});

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
      'SyncStatus(conn: $conn, connected: $connected, lastSyncedAt: $lastSyncedAt)';
}
