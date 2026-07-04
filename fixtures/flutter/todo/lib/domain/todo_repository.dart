class Todo {
  const Todo({required this.id, required this.title, this.done = false});
  final String id;
  final String title;
  final bool done;
}

abstract interface class TodoRepository {
  Stream<List<Todo>> watch();
  Future<void> add(String title);
  Future<void> toggle(String id);
}
