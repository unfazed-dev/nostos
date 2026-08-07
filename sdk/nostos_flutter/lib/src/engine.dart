/// The seam between the public [NostosEngine]-typed API in `nostos.dart` and
/// the generated flutter_rust_bridge bindings.
///
/// Why this exists: the generated `NostosHandle.subscribe` takes a
/// `RustStreamSink<T>`, whose `.stream` getter only works after the FFI call
/// has run `setupAndSerialize` on it (see
/// `flutter_rust_bridge/src/stream/stream_sink.dart`) — a pure-Dart test
/// double cannot satisfy that type without loading the native library. This
/// interface deals only in plain Dart `Stream`s, so a unit test can inject a
/// [NostosEngine] fake and exercise `Nostos`'s API surface (subscribe/watch/
/// write wiring, error paths, table-mismatch checks) with zero native
/// dependency. [RustNostosEngine] is the one adapter that actually talks to
/// Rust; it's the only file in this package that imports
/// `src/rust/api/nostos.dart`.
library;

import 'rust/api/nostos.dart' as rust;

// Re-exported so the public API in `nostos.dart` (`Nostos.applySchema`) and
// `schema.dart` (`NostosSchema.toClientTables`) can name [ClientTableFfi] without
// each importing `rust/api/nostos.dart` directly — keeping this file the sole
// importer of the generated bindings (see the library doc above).
export 'rust/api/nostos.dart' show ClientTableFfi;

/// Connection-state transitions, decoupled from the generated
/// `rust.NostosConnectionState` so consumers never need to import generated
/// code. See `rust/src/api/nostos.rs`'s `NostosConnectionState` doc for the
/// precise (heuristic) semantics of `connected`.
enum NostosConnectionState { connecting, connected, reconnecting, disconnected }

/// One table in a multi-table subscription: a name + an optional safe-SQL
/// `where_sql` (ADR-0012). A connection subscribes to a list of these over
/// one `/sync` socket (D1/ADR-0022 multi-table-per-handle). Plain Dart (not
/// the generated `TableSubFfi`) so tests can build it without the native
/// library.
class NostosTableSub {
  const NostosTableSub({required this.name, this.whereSql});

  /// NostosTable name to subscribe to.
  final String name;

  /// Optional safe-SQL predicate scoped to this table (ADR-0012).
  final String? whereSql;
}

/// What [Nostos] needs from a backend. Implemented for real by [RustNostosEngine];
/// implement it yourself in tests to avoid the native library entirely.
abstract class NostosEngine {
  /// Start a multi-table subscription over one `/sync` socket. Returns the
  /// connection-state stream (the session's lifecycle). Call [watch] per
  /// table to receive that table's rows.
  Stream<NostosConnectionState> subscribe({required List<NostosTableSub> tables});

  /// Attach a row stream for one subscribed table: one JSON-array-of-objects
  /// string per tick (the durable snapshot immediately, then after every
  /// applied change). `table` must be among those passed to [subscribe].
  Stream<String> watch({required String table});

  /// Durable-outbox status: queued writes, permanently-failed writes, and the
  /// server's message for the last permanent failure.
  ///
  /// A record rather than a class on purpose: this is internal plumbing between
  /// the engine and [SyncStatus] (the type apps actually bind to), so a named
  /// type here would be a third spelling of the same three fields.
  ///
  /// Emits the current value immediately on listen. `lastError` is set only for
  /// a *permanently* failed write — ordinary rejections retry and stay silent.
  Stream<({int pending, int deadLettered, String? lastError})> watchWriteStatus();

  /// Returns the local outbox id.
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  });

  /// Atomic batch enqueue — all ops land in one storage transaction or none
  /// do (ADR-0032 T3). Returns outbox ids in the same order as `ops`.
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})> ops,
  });

  /// Add an element to an OR-set row (ADR-0030 / ADR-0032 T4). Returns the
  /// outbox id.
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  });

  /// Remove an element from an OR-set row (tombstone). Returns the outbox id.
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  });

  /// Increment the PN-Counter in row [pk] of [table] by [delta] (ADR-0030
  /// addendum). Returns the outbox id.
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  });

  /// Decrement the PN-Counter by [delta] (bumps the negative counter).
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  });

  /// Run an arbitrary SELECT against on-device SQLite. Returns a JSON-array
  /// string (same shape as [watch]'s ticks); decode with jsonDecode. Requires
  /// an active subscription.
  Future<String> query({required String sql});

  /// Materialize the WS2 read-views for [tables] in the on-device SQLite
  /// file (`CREATE VIEW IF NOT EXISTS <table> AS SELECT json_extract(...)
  /// ... FROM cairn_data WHERE table_name='<table>'` — see
  /// `SqliteStorage::apply_schema`). Idempotent for an unchanged schema;
  /// the views persist in the SQLite file, so this only needs to run once
  /// after connect. Synchronous: the FFI is `Result<(), String>` and throws
  /// on error. Wraps the generated `NostosHandle.applySchema`.
  void applySchema(List<rust.ClientTableFfi> tables);

  /// Pause syncing: abort only the connect loop; reads, writes (durable outbox),
  /// and `watch` pumps keep working offline. Pair with [resume]. Idempotent.
  /// Replace the bearer token used by subsequent connections.
  ///
  /// Tears nothing down: a live socket keeps running and the next connection
  /// picks the new token up. That is the point — rebuilding the handle to change
  /// a token would end every `watch` stream the UI is holding.
  Future<void> setToken(String? token);

  Future<void> disconnect();

  /// Resume syncing after [disconnect]: respawn the connect loop on the same
  /// client (outbox flushes on reconnect). Returns the fresh connection-state
  /// stream; `Nostos` pipes it into its public `connectionState`.
  Stream<NostosConnectionState> resume();

  /// Tear down the active subscription's background work (the sync loop and
  /// every watch pump). Safe to call with no active subscription and safe to
  /// call more than once.
  Future<void> close();

  /// ADR-0029: sign out — wipe local rows + durable outbox, stop sync, clear
  /// the seed token. Unlike [close], the on-device SQLite state is wiped via
  /// `clear_local_state`. Idempotent.
  Future<void> signOut();
}

/// The real engine: wraps the generated `rust.NostosHandle`.
class RustNostosEngine implements NostosEngine {
  RustNostosEngine._(this._handle);

  /// Opens a connection (no network activity yet — see `NostosHandle.connect`
  /// in the Rust glue).
  factory RustNostosEngine.connect({
    required String url,
    String? token,
    required String dbPath,
  }) => RustNostosEngine._(
    rust.NostosHandle.connect(url: url, token: token, dbPath: dbPath),
  );

  final rust.NostosHandle _handle;

  @override
  Stream<NostosConnectionState> subscribe({required List<NostosTableSub> tables}) {
    final ffiTables = tables
        .map((t) => rust.TableSubFfi(name: t.name, whereSql: t.whereSql))
        .toList(growable: false);
    return _handle.subscribe(tables: ffiTables).map(_mapState);
  }

  @override
  Stream<String> watch({required String table}) => _handle.watch(table: table);

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
      watchWriteStatus() => _handle.watchWriteStatus().map(
            (s) => (
              pending: s.pending.toInt(),
              deadLettered: s.deadLettered.toInt(),
              lastError: s.lastError,
            ),
          );

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    final id = await _handle.write(
      table: table,
      op: op,
      pk: pk,
      payloadJson: payloadJson,
    );
    return id.toInt();
  }

  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})> ops,
  }) async {
    final inputs = ops
        .map((o) => rust.NostosWriteInput(
              table: o.table,
              op: o.op,
              pk: o.pk,
              payloadJson: o.payloadJson,
            ))
        .toList();
    final ids = await _handle.writeBatch(ops: inputs);
    return ids.map((b) => b.toInt()).toList();
  }

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) =>
      _handle.orSetAdd(table: table, pk: pk, element: element).then((b) => b.toInt());

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) =>
      _handle.orSetRemove(table: table, pk: pk, element: element).then((b) => b.toInt());

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) =>
      _handle
          .counterIncrement(table: table, pk: pk, delta: delta)
          .then((b) => b.toInt());

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) =>
      _handle
          .counterDecrement(table: table, pk: pk, delta: BigInt.from(delta))
          .then((b) => b.toInt());

  @override
  Future<String> query({required String sql}) =>
      _handle.query(sql: sql);

  @override
  void applySchema(List<rust.ClientTableFfi> tables) =>
      _handle.applySchema(tables: tables);

  @override
  Future<void> setToken(String? token) => _handle.setToken(token: token);

  @override
  Future<void> close() => _handle.close();

  @override
  Future<void> signOut() => _handle.signOut();

  @override
  Future<void> disconnect() => _handle.disconnect();

  @override
  Stream<NostosConnectionState> resume() => _handle.resume().map(_mapState);
}

NostosConnectionState _mapState(rust.NostosConnectionState s) => switch (s) {
  rust.NostosConnectionState.connecting => NostosConnectionState.connecting,
  rust.NostosConnectionState.connected => NostosConnectionState.connected,
  rust.NostosConnectionState.reconnecting => NostosConnectionState.reconnecting,
  rust.NostosConnectionState.disconnected => NostosConnectionState.disconnected,
};
