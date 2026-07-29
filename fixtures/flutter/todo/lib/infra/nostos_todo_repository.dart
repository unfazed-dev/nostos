import 'dart:async';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/foundation.dart';
import 'package:path_provider/path_provider.dart';
import 'package:todo/domain/todo_repository.dart';

/// Nostos-backed [TodoRepository] — the W5 showcase over the reactive facade
/// (ADR-0024: `NostosDatabase` + `Collection<T>`). Only constructed when
/// [Env.isNostosLive]; the mock app uses [InMemoryTodoRepository], the
/// Supabase-direct app uses [SupabaseTodoRepository].
///
/// Deliberately subscribes to `todos` WITHOUT a `where` filter: tenant
/// enforcement (ADR-0011 reads, ADR-0018 writes) scopes rows to the
/// authenticated principal's tenant server-side, so the client never has to
/// (and never gets to) express "give me my rows" — it asks for `todos` and
/// the server hands back only the caller's own. Likewise [add] never sends a
/// `user_id`: the server force-stamps it from the JWT `sub`, and any
/// client-claimed value is silently overwritten (ADR-0018).
///
/// Writes ([add]/[toggle]/[update]/[remove]) go through [Collection]'s
/// collapsed-write outbox — the call returns as soon as the write is
/// captured on disk, not once the server acks it, so the UI never blocks on
/// connectivity (offline CRUD all work; they sync when the socket
/// reconnects). See the SDK README's "Known gaps": a write the server rejects
/// (e.g. a cross-tenant attempt) has NO surface back to Dart today — it just
/// stays queued and retries silently at the `nostos-client` outbox layer.
/// This repository cannot detect that case; ADR-0018 isolation is proven at
/// the server/Postgres boundary instead (see
/// integration_test/nostos_live_test.dart).
class NostosTodoRepository implements TodoRepository {
  NostosTodoRepository._(this._db, this._collection);

  final NostosDatabase _db;
  final Collection<Todo> _collection;
  static const _table = 'todos';

  /// The declared read-view schema — re-applied on every connect (the
  /// migration story, see [NostosSchema]). The WS2 view projects these three
  /// columns from `cairn_data.payload` via `json_extract`; the server's
  /// `todos` table also carries `user_id`/`created_at`, which stay in the
  /// payload JSON unprojected (the read model is a projection, not a mirror).
  static final NostosSchema _schema = NostosSchema(tables: [
    NostosTable(
      name: _table,
      primaryKey: const ['id'],
      columns: const [
        NostosColumn.text('id'),
        NostosColumn.text('title'),
        NostosColumn.integer('done'),
      ],
    ),
  ]);

  /// The last row set [watch] emitted, keyed by id — used by [toggle] to
  /// flip `done` without a network round-trip (mirrors the read-then-write
  /// shape [SupabaseTodoRepository] uses, but from the local reactive cache
  /// instead of a fresh query).
  final Map<String, Todo> _lastById = {};

  /// Connects, declares the [todos] schema (creating the WS2 read-view),
  /// subscribes to `todos` (no filter — see class doc), and returns a ready
  /// repository. [wsUrl]/[token] come from [Env.nostosWsUrl]/[Env.nostosToken]
  /// in the app; the integration test passes its own. [sqlitePath] overrides
  /// the default per-app local store location — used by the offline-persistence
  /// scenario to reopen the same durable store across a fresh repository
  /// instance.
  static Future<NostosTodoRepository> connect({
    required String wsUrl,
    required String token,
    String? sqlitePath,
  }) async {
    final path = sqlitePath ?? await _defaultSqlitePath();
    final db = await NostosDatabase.connect(
      url: wsUrl,
      token: token,
      schema: _schema,
      sqlitePath: path,
    );
    await db.subscribe(_table);
    return NostosTodoRepository._(
      db,
      db.collection<Todo>(
        table: _table,
        fromRow: Todo.fromJson,
        toRow: (t) => t.toJson(),
        pkColumn: 'id',
      ),
    );
  }

  static Future<String> _defaultSqlitePath() async {
    final dir = await getApplicationSupportDirectory();
    return '${dir.path}/nostos_todo.sqlite';
  }

  @override
  Stream<List<Todo>> watch() {
    return _collection.watch().map((todos) {
      _lastById
        ..clear()
        ..addEntries(todos.map((t) => MapEntry(t.id, t)));
      return todos;
    });
  }

  @override
  Future<void> add(String title) async {
    final id = DateTime.now().microsecondsSinceEpoch.toString();
    await _collection.upsertRow({'id': id, 'title': title, 'done': false});
  }

  @override
  Future<void> toggle(String id) async {
    final current = _lastById[id];
    final next = !(current?.done ?? false);
    // Deliberately omit `title` from the payload: the server's ON CONFLICT
    // SET only touches columns present in the payload, so an omitted `title`
    // is left unchanged rather than overwritten with an empty value.
    await _collection.patch(id, {'done': next});
  }

  @override
  Future<void> update(String id, {String? title, bool? done}) async {
    final cols = <String, dynamic>{};
    if (title != null) cols['title'] = title;
    if (done != null) cols['done'] = done;
    if (cols.isEmpty) return;
    await _collection.patch(id, cols);
  }

  @override
  Future<void> remove(String id) async {
    await _collection.delete(id);
  }

  /// Hot sync status (ADR-0024). Drives the UI's offline banner via
  /// [TodoViewModel.currentStatus]. `conn == connected` is the online
  /// signal; `lastSyncedAt` stamps the last transition to connected.
  ValueListenable<SyncStatus> get status => _db.status;

  @override
  Future<void> dispose() => _db.close();
}
