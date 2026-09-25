/// The direct-mode [NostosEngine] adapter: wraps the generated
/// `rust.NostosDirectHandle`, which talks to a Supabase project with no
/// `nostos-server` anywhere (`docs/plans/direct-mode-sync-protocol.md`).
///
/// Like `engine_io.dart` this file is native-only — it is reached solely from
/// `engine_selector_io.dart`, so `flutter build web` never sees frb's io
/// signatures (ADR-0036's split; see that file's doc for why the two cannot
/// share a body).
///
/// Three verbs of [NostosEngine] have no meaning without a server, and they
/// throw [UnsupportedError] rather than silently doing nothing:
/// [subscribeStream] / [unsubscribeStream] (a stream is a server-held
/// predicate) and [orSetAdd] / [orSetRemove] (add-wins merge is the server's
/// job). A `whereSql` on a subscribed table throws for the same reason: in
/// direct mode the predicate lives in RLS, in the database, not in the client's
/// request.
library;

import 'engine.dart';
import 'rust/api/nostos.dart' as rust;
import 'rust/api/direct.dart' as direct;

/// Talks straight to Postgres: PostgREST for pull/push, Realtime for the
/// doorbell, RLS for the authorization.
class DirectNostosEngine implements NostosEngine {
  DirectNostosEngine._(this._handle, this._scope, this._counterFields);

  /// Opens the device database and points it at a Supabase project. No network
  /// activity until [subscribe].
  ///
  /// [scope] is the value the change-log trigger stamps and the private
  /// Realtime channel this device may join — `sub:<user-uuid>` for a
  /// user-scoped app.
  ///
  /// [counterFields] maps a table to the column `nostos_increment` should add
  /// to, and is what makes [counterIncrement] expressible: server mode gets
  /// that column from `NOSTOS_COUNTER_COLUMNS`, and direct mode has no server to
  /// read it from, so the app declares it here.
  ///
  /// [keepLocalOnSignOut] (ADR-0049): `false` wipes the device on [signOut]
  /// (ADR-0029, the default). `true` keeps the rows across sign-out so the
  /// same user's next sign-in resumes instead of re-downloading; a different
  /// user's token wipes before its first pull.
  factory DirectNostosEngine.connect({
    required String supabaseUrl,
    required String anonKey,
    required String scope,
    String? token,
    required String dbPath,
    Map<String, String> counterFields = const <String, String>{},
    bool keepLocalOnSignOut = false,
  }) => DirectNostosEngine._(
    direct.NostosDirectHandle.connect(
      supabaseUrl: supabaseUrl,
      anonKey: anonKey,
      token: token,
      dbPath: dbPath,
      keepLocalOnSignOut: keepLocalOnSignOut,
    ),
    scope,
    counterFields,
  );

  final direct.NostosDirectHandle _handle;
  final String _scope;
  final Map<String, String> _counterFields;

  /// Start the sync loop. [tables] is used only to validate the call: there is
  /// no subscription frame to send — what this device may read is whatever its
  /// JWT gets past RLS.
  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) {
    final filtered = tables.where((t) => t.whereSql != null).map((t) => t.name);
    if (filtered.isNotEmpty) {
      throw UnsupportedError(
        'direct mode: whereSql on ${filtered.join(", ")} has nowhere to run — '
        'the row filter is an RLS policy on the table, not a client predicate. '
        'Express it in nostos_rules.toml and re-run `nostos link --mode direct`.',
      );
    }
    if (orSetTables.isNotEmpty) {
      throw UnsupportedError(
        'direct mode: OR-set tables (${orSetTables.join(", ")}) need the '
        "server's add-wins merge. Only counters are serializable without one "
        '(nostos_increment).',
      );
    }
    return _handle.start(scope: _scope).map(_mapState);
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
  Future<String> subscribeStream({
    required String name,
    required String paramsJson,
  }) => throw UnsupportedError(
    'direct mode: sync streams are server-held predicate templates '
    '(nostos_rules.toml [streams.$name]) and there is no server to hold them.',
  );

  @override
  Future<void> unsubscribeStream({required String id}) =>
      throw UnsupportedError('direct mode: sync streams are not available.');

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
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async {
    final inputs = ops
        .map(
          (o) => rust.NostosWriteInput(
            table: o.table,
            op: o.op,
            pk: o.pk,
            payloadJson: o.payloadJson,
          ),
        )
        .toList();
    final ids = await _handle.writeBatch(ops: inputs);
    return ids.map((b) => b.toInt()).toList();
  }

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) => throw UnsupportedError(_noOrSet);

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) => throw UnsupportedError(_noOrSet);

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) => _increment(table, pk, delta);

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) => _increment(table, pk, -delta);

  Future<int> _increment(String table, String pk, int delta) async {
    final field = _counterFields[table];
    if (field == null) {
      throw StateError(
        'counter table "$table" was not declared: pass '
        'counterFields: {"$table": "<column>"} to DirectNostosEngine.connect. '
        'Without the column name nostos_increment has nothing to add to.',
      );
    }
    final id = await _handle.increment(
      table: table,
      pk: pk,
      field: field,
      delta: delta.toDouble(),
    );
    return id.toInt();
  }

  @override
  Future<String> query({required String sql}) => _handle.query(sql: sql);

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

  /// Native storage is always durable — the degrade signal is web-only.
  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();
}

const String _noOrSet =
    'direct mode: OR-sets need the server-side add-wins merge. Model the set '
    'as rows and delete them, or run a nostos-server for this table.';

NostosConnectionState _mapState(rust.NostosConnectionState s) => switch (s) {
  rust.NostosConnectionState.connecting => NostosConnectionState.connecting,
  rust.NostosConnectionState.connected => NostosConnectionState.connected,
  rust.NostosConnectionState.reconnecting => NostosConnectionState.reconnecting,
  rust.NostosConnectionState.disconnected => NostosConnectionState.disconnected,
};
