import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:todo/domain/todo_repository.dart';

/// Nostos-backed [TodoRepository] — the W5 showcase. Only constructed when
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
/// Writes go through [Nostos.write]'s durable local outbox — the call returns
/// as soon as the write is captured on disk, not once the server acks it, so
/// the UI never blocks on connectivity (offline create/toggle both work; they
/// sync when the socket reconnects). See the SDK README's "Known gaps": a
/// write the server rejects (e.g. a cross-tenant attempt) has NO surface back
/// to Dart today — it just stays queued and retries forever, silently, at the
/// `nostos-client` outbox layer. This repository cannot detect that case; ADR-
/// 0018 isolation is proven at the server/Postgres boundary instead (see
/// integration_test/nostos_live_test.dart).
class NostosTodoRepository implements TodoRepository {
  NostosTodoRepository._(this._nostos);

  final Nostos _nostos;
  static const _table = 'todos';

  /// The last row set [watch] emitted, keyed by id — used by [toggle] to
  /// flip `done` without a network round-trip (mirrors the read-then-write
  /// shape [SupabaseTodoRepository] uses, but from the local reactive cache
  /// instead of a fresh query — there's no ad-hoc query API on [Nostos]).
  final Map<String, Todo> _lastById = {};

  /// Connects, subscribes to `todos` (no filter — see class doc), and
  /// returns a ready repository. [wsUrl]/[token] come from [Env.nostosWsUrl]/
  /// [Env.nostosToken] in the app; the integration test passes its own.
  /// [sqlitePath] overrides the default per-url local store location — used
  /// by the offline-persistence scenario to reopen the same durable store
  /// across a fresh `Nostos` instance (see [Nostos.connect]).
  static Future<NostosTodoRepository> connect({
    required String wsUrl,
    required String token,
    String? sqlitePath,
  }) async {
    final nostos = await Nostos.connect(url: wsUrl, token: token, sqlitePath: sqlitePath);
    await nostos.subscribe(_table);
    return NostosTodoRepository._(nostos);
  }

  @override
  Stream<List<Todo>> watch() {
    return _nostos.watch(_table).map((rows) {
      final todos = rows.map(_toTodo).toList();
      _lastById
        ..clear()
        ..addEntries(todos.map((t) => MapEntry(t.id, t)));
      return todos;
    });
  }

  @override
  Future<void> add(String title) async {
    final id = DateTime.now().microsecondsSinceEpoch.toString();
    await _nostos.write(
      _table,
      op: 'upsert',
      pk: id,
      payload: {'title': title, 'done': false},
    );
  }

  @override
  Future<void> toggle(String id) async {
    final current = _lastById[id];
    final next = !(current?.done ?? false);
    // Deliberately omit `title` from the payload: the server's ON CONFLICT
    // SET only touches columns present in the payload, so an omitted `title`
    // is left unchanged rather than overwritten with an empty value.
    await _nostos.write(_table, op: 'upsert', pk: id, payload: {'done': next});
  }

  /// A row's payload always carries every column for a Postgres-backed
  /// deployment (including `id`); `_pk` (stamped client-side by the SDK) is
  /// the fallback for any payload shape that omits it.
  static Todo _toTodo(Map<String, dynamic> row) {
    final id = (row['id'] ?? row['_pk']) as String;
    return Todo(
      id: id,
      title: row['title'] as String? ?? '',
      done: _asBool(row['done']),
    );
  }

  /// `PgReplicator::tuple_to_json_payload` (crates/nostos-infra) now renders
  /// a Postgres `boolean` column as a real JSON bool (ADR-0019's OID-keyed
  /// mapping) — a real Postgres source delivers `"done":true`, matching a
  /// mock/fake source. The `String` arm below is kept as defensive
  /// passthrough (e.g. for a hand-rolled test payload or a future non-pg
  /// source using the pre-ADR-0019 shape), not because the real wire needs
  /// it anymore.
  static bool _asBool(Object? value) => switch (value) {
    bool b => b,
    String s => s == 'true',
    _ => false,
  };
}
