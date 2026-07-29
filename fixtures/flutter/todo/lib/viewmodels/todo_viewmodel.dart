import 'dart:async';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/foundation.dart';

import '../domain/todo_repository.dart';
import '../infra/nostos_todo_repository.dart';

/// Todo list view-state over the [TodoRepository] port. Subscribes to
/// [TodoRepository.watch] and re-renders on every emission. Mocked in tests
/// via a mocktail [TodoRepository]; backed by [InMemoryTodoRepository] (mock
/// mode), [SupabaseTodoRepository] (Supabase-direct), or
/// [NostosTodoRepository] (Nostos sync — the W5 showcase) in the app.
///
/// When the backend is [NostosTodoRepository], the view-model ALSO forwards
/// [currentStatus] (ADR-0024 [SyncStatus]) so the UI can render an offline
/// banner; for the other backends [currentStatus] stays null and the banner
/// is hidden.
class TodoViewModel extends ChangeNotifier {
  TodoViewModel(this._todos) {
    _sub = _todos.watch().listen(_onData);
    if (_todos is NostosTodoRepository) {
      _status = _todos.status;
      _status!.addListener(_onStatus);
    }
  }

  final TodoRepository _todos;
  StreamSubscription<List<Todo>>? _sub;

  // ADR-0024 sync status — only wired when the backend is Nostos.
  ValueListenable<SyncStatus>? _status;

  List<Todo> _todosState = const [];
  List<Todo> get todos => _todosState;

  SyncStatus? _statusState;
  /// Current sync status, or null when the backend has no notion of
  /// connectivity (mock + Supabase-direct). UI uses this for the banner:
  /// `currentStatus?.connected == false` ⇒ show "offline".
  SyncStatus? get currentStatus => _statusState;

  void _onData(List<Todo> next) {
    _todosState = next;
    notifyListeners();
  }

  void _onStatus() {
    _statusState = _status!.value;
    notifyListeners();
  }

  /// Adds a todo. Trims the title first; empty/whitespace-only titles are
  /// ignored (no delegation to the repository).
  Future<void> add(String title) async {
    final trimmed = title.trim();
    if (trimmed.isEmpty) return;
    await _todos.add(trimmed);
  }

  /// Toggles the completion flag of the todo with the given id.
  Future<void> toggle(String id) => _todos.toggle(id);

  /// Partial update — only the non-null fields are written.
  Future<void> update(String id, {String? title, bool? done}) =>
      _todos.update(id, title: title, done: done);

  /// Deletes the todo with the given id.
  Future<void> remove(String id) => _todos.remove(id);

  @override
  void dispose() {
    _sub?.cancel();
    _status?.removeListener(_onStatus);
    _todos.dispose();
    super.dispose();
  }
}
