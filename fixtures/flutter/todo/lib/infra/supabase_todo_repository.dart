import 'package:supabase_flutter/supabase_flutter.dart';
import 'package:todo/domain/todo_repository.dart';

/// Supabase-backed [TodoRepository]. Only constructed when [Env.isLive] — the
/// mock app uses [InMemoryTodoRepository]. Verified against supabase_flutter
/// 2.15.4: `.stream(primaryKey: ['id'])` returns a
/// `Stream<List<Map<String, dynamic>>>` (typed [SupabaseStreamEvent]), and
/// `.order(...)` chains on the stream builder. Writes are scoped to the
/// current user; RLS (see supabase/schema.sql) isolates rows to their owner.
class SupabaseTodoRepository implements TodoRepository {
  SupabaseTodoRepository(this._client);
  final SupabaseClient _client;

  @override
  Stream<List<Todo>> watch() {
    return _client
        .from('todos')
        .stream(primaryKey: ['id'])
        .order('created_at')
        .map((rows) => rows
            .map((row) => Todo(
                  id: row['id'] as String,
                  title: row['title'] as String,
                  done: (row['done'] as bool?) ?? false,
                ))
            .toList());
  }

  @override
  Future<void> add(String title) async {
    await _client.from('todos').insert({
      'title': title,
      'user_id': _client.auth.currentUser!.id,
    });
  }

  @override
  Future<void> toggle(String id) async {
    // Read-then-write: fine for a fixture; RLS guarantees the row (if present)
    // belongs to the current user.
    final res = await _client
        .from('todos')
        .select('done')
        .eq('id', id)
        .limit(1)
        .maybeSingle();
    final current = (res?['done'] as bool?) ?? false;
    await _client.from('todos').update({'done': !current}).eq('id', id);
  }

  @override
  Future<void> update(String id, {String? title, bool? done}) async {
    final patch = <String, dynamic>{};
    if (title != null) patch['title'] = title;
    if (done != null) patch['done'] = done;
    if (patch.isEmpty) return;
    await _client.from('todos').update(patch).eq('id', id);
  }

  @override
  Future<void> remove(String id) async {
    await _client.from('todos').delete().eq('id', id);
  }

  @override
  Future<void> dispose() async {}
}
