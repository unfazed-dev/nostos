class Todo {
  const Todo({required this.id, required this.title, this.done = false});
  final String id;
  final String title;
  final bool done;

  /// Decodes a row from either backend's read path. Nostos's WS2 read-view
  /// projects `done` via `json_extract`, which renders a JSON `true`/`false`
  /// as a SQLite INTEGER (`1`/`0`); Supabase streams deliver a real `bool`;
  /// hand-rolled test payloads sometimes use the string `'true'`. All three
  /// shapes are normalized by [_asBool].
  factory Todo.fromJson(Map<String, dynamic> row) => Todo(
        id: (row['id'] ?? row['_pk']) as String,
        title: row['title'] as String? ?? '',
        done: _asBool(row['done']),
      );

  /// Full-row image for collapsed upsert writes (ADR-0013). The server
  /// force-stamps `user_id`/`created_at` from the JWT — they are deliberately
  /// NOT in the write payload (see [NostosTodoRepository]'s class doc).
  Map<String, dynamic> toJson() => {
        'id': id,
        'title': title,
        'done': done,
      };

  static bool _asBool(Object? value) => switch (value) {
        bool b => b,
        int i => i != 0,
        String s => s == 'true',
        _ => false,
      };
}

/// Reactive CRUD port over the todos table. Implementations:
/// [InMemoryTodoRepository] (mock mode), [SupabaseTodoRepository]
/// (Supabase-direct), [NostosTodoRepository] (Nostos sync — the W5 showcase).
abstract interface class TodoRepository {
  Stream<List<Todo>> watch();

  Future<void> add(String title);

  Future<void> toggle(String id);

  /// Partial update: only the non-null fields are written. No-round-trip
  /// patch semantics — columns absent from the payload are left untouched
  /// (server-authoritative per-field LWW, ADR-0014).
  Future<void> update(String id, {String? title, bool? done});

  /// Delete the row with the given id.
  Future<void> remove(String id);

  /// Release backend resources (sync session, stream controllers). The
  /// view-model calls this once from its [ChangeNotifier.dispose].
  Future<void> dispose();
}
