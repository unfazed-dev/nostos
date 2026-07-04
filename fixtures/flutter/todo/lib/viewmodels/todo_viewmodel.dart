import 'dart:async';

import 'package:flutter/foundation.dart';

import '../domain/todo_repository.dart';

/// Todo list view-state over the [TodoRepository] port. Subscribes to
/// [TodoRepository.watch] and re-renders on every emission. Mocked in tests
/// via a mocktail [TodoRepository]; backed by [InMemoryTodoRepository] (mock
/// mode) or [SupabaseTodoRepository] (live mode) in the app.
class TodoViewModel extends ChangeNotifier {
  TodoViewModel(this._todos) {
    _sub = _todos.watch().listen(_onData);
  }

  final TodoRepository _todos;
  StreamSubscription<List<Todo>>? _sub;

  List<Todo> _todosState = const [];
  List<Todo> get todos => _todosState;

  void _onData(List<Todo> next) {
    _todosState = next;
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

  @override
  void dispose() {
    _sub?.cancel();
    super.dispose();
  }
}
