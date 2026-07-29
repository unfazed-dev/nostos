import 'dart:async';

import 'package:todo/domain/todo_repository.dart';

/// In-memory [TodoRepository]. Holds a list behind a broadcast
/// [StreamController] and re-emits the current list on every mutation and on
/// every new listener. Used in mock mode (the default) and the persona smoke.
class InMemoryTodoRepository implements TodoRepository {
  final List<Todo> _todos = [];
  final StreamController<List<Todo>> _controller =
      StreamController<List<Todo>>.broadcast();

  @override
  Stream<List<Todo>> watch() {
    // Emit the current snapshot to a new listener immediately, then forward
    // every subsequent mutation from the broadcast controller.
    return Stream<List<Todo>>.multi((controller) {
      controller.add(List.unmodifiable(_todos));
      _controller.stream.listen(
        controller.add,
        onError: controller.addError,
        onDone: controller.close,
      );
    });
  }

  @override
  Future<void> add(String title) async {
    final todo = Todo(
      id: DateTime.now().microsecondsSinceEpoch.toString(),
      title: title,
    );
    _todos.add(todo);
    _emit();
  }

  @override
  Future<void> toggle(String id) async {
    final i = _todos.indexWhere((t) => t.id == id);
    if (i == -1) return;
    final old = _todos[i];
    _todos[i] = Todo(id: old.id, title: old.title, done: !old.done);
    _emit();
  }

  @override
  Future<void> update(String id, {String? title, bool? done}) async {
    final i = _todos.indexWhere((t) => t.id == id);
    if (i == -1) return;
    final old = _todos[i];
    _todos[i] = Todo(
      id: old.id,
      title: title ?? old.title,
      done: done ?? old.done,
    );
    _emit();
  }

  @override
  Future<void> remove(String id) async {
    _todos.removeWhere((t) => t.id == id);
    _emit();
  }

  @override
  Future<void> dispose() async {
    await _controller.close();
  }

  void _emit() {
    _controller.add(List.unmodifiable(_todos));
  }
}
