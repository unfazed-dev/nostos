/// Native Appwrite Cloud transport over Nostos' SQLite apply engine.
library;

import 'engine.dart';
import 'rust/api/appwrite.dart' as appwrite;
import 'rust/api/nostos.dart' as rust;

class AppwriteNostosEngine implements NostosEngine {
  AppwriteNostosEngine._(this._handle);

  factory AppwriteNostosEngine.connect({
    required String endpoint,
    required String projectId,
    required String functionId,
    required String userId,
    required String jwt,
    required String dbPath,
  }) => AppwriteNostosEngine._(
    appwrite.NostosAppwriteHandle.connect(
      endpoint: endpoint,
      projectId: projectId,
      functionId: functionId,
      userId: userId,
      jwt: jwt,
      dbPath: dbPath,
    ),
  );

  final appwrite.NostosAppwriteHandle _handle;

  @override
  Stream<NostosConnectionState> subscribe({
    required List<NostosTableSub> tables,
    Set<String> orSetTables = const <String>{},
    Set<String> counterTables = const <String>{},
  }) {
    if (tables.any((table) => table.whereSql != null)) {
      throw UnsupportedError(
        'Appwrite visibility is enforced by the Function.',
      );
    }
    if (orSetTables.isNotEmpty || counterTables.isNotEmpty) {
      throw UnsupportedError(
        'Appwrite does not support Nostos CRDT writes yet.',
      );
    }
    return _handle.start().map(_mapState);
  }

  @override
  Stream<String> watch({required String table}) => _handle.watch(table: table);

  @override
  Stream<({int pending, int deadLettered, String? lastError})>
  watchWriteStatus() => _handle.watchWriteStatus().map(
    (status) => (
      pending: status.pending.toInt(),
      deadLettered: status.deadLettered.toInt(),
      lastError: status.lastError,
    ),
  );

  @override
  Future<String> subscribeStream({
    required String name,
    required String paramsJson,
  }) =>
      throw UnsupportedError('Appwrite Functions have no server sync streams.');

  @override
  Future<void> unsubscribeStream({required String id}) =>
      throw UnsupportedError('Appwrite Functions have no server sync streams.');

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async => (await _handle.write(
    table: table,
    op: op,
    pk: pk,
    payloadJson: payloadJson,
  )).toInt();

  @override
  Future<List<int>> writeBatch({
    required List<({String table, String op, String pk, String? payloadJson})>
    ops,
  }) async {
    final inputs = ops
        .map(
          (op) => rust.NostosWriteInput(
            table: op.table,
            op: op.op,
            pk: op.pk,
            payloadJson: op.payloadJson,
          ),
        )
        .toList();
    return (await _handle.writeBatch(ops: inputs))
        .map((id) => id.toInt())
        .toList();
  }

  @override
  Future<int> orSetAdd({
    required String table,
    required String pk,
    required String element,
  }) => throw UnsupportedError('Appwrite OR-set writes are unavailable.');

  @override
  Future<int> orSetRemove({
    required String table,
    required String pk,
    required String element,
  }) => throw UnsupportedError('Appwrite OR-set writes are unavailable.');

  @override
  Future<int> counterIncrement({
    required String table,
    required String pk,
    required int delta,
  }) => throw UnsupportedError('Appwrite counter writes are unavailable.');

  @override
  Future<int> counterDecrement({
    required String table,
    required String pk,
    required int delta,
  }) => throw UnsupportedError('Appwrite counter writes are unavailable.');

  @override
  Future<String> query({required String sql}) => _handle.query(sql: sql);

  @override
  void applySchema(List<rust.ClientTableFfi> tables) =>
      _handle.applySchema(tables: tables);

  @override
  Future<void> setToken(String? token) => _handle.setToken(token: token);

  Future<void> setUser({required String userId, required String jwt}) =>
      _handle.setUser(userId: userId, jwt: jwt);

  Future<int> syncNow() async => (await _handle.syncNow()).toInt();

  @override
  Future<void> disconnect() => _handle.disconnect();

  @override
  Stream<NostosConnectionState> resume() => _handle.resume().map(_mapState);

  @override
  Future<void> signOut() => _handle.signOut();

  @override
  Future<void> close() => _handle.close();

  @override
  Stream<bool> get webStorageDegraded => const Stream<bool>.empty();
}

NostosConnectionState _mapState(
  rust.NostosConnectionState state,
) => switch (state) {
  rust.NostosConnectionState.connecting => NostosConnectionState.connecting,
  rust.NostosConnectionState.connected => NostosConnectionState.connected,
  rust.NostosConnectionState.reconnecting => NostosConnectionState.reconnecting,
  rust.NostosConnectionState.disconnected => NostosConnectionState.disconnected,
  rust.NostosConnectionState.accessRevoked =>
    NostosConnectionState.accessRevoked,
};
