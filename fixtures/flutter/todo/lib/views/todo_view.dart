import 'package:flutter/material.dart';

import '../viewmodels/auth_viewmodel.dart';
import '../viewmodels/todo_viewmodel.dart';

/// Todo home: a new-todo [TextField] (`todos.input`) + add [IconButton]
/// (`todos.add`) + a [ListView] (`todos.list`) of [CheckboxListTile]s + a
/// sign-out [IconButton] (`auth.signout`). Task 10's smoke drives all four.
///
/// The view is always constructed with parent-owned VMs ([authVm] + [todoVm]);
/// it never disposes them — [TodoApp] owns their lifecycle.
class TodoView extends StatefulWidget {
  const TodoView({super.key, required this.authVm, required this.todoVm});

  final AuthViewModel authVm;
  final TodoViewModel todoVm;

  @override
  State<TodoView> createState() => _TodoViewState();
}

class _TodoViewState extends State<TodoView> {
  late final TextEditingController _newTodo = TextEditingController();

  Future<void> _add() => widget.todoVm.add(_newTodo.text);

  @override
  void dispose() {
    _newTodo.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(
          title: const Text('Todos'),
          actions: [
            IconButton(
              key: const Key('auth.signout'),
              icon: const Icon(Icons.logout),
              tooltip: 'Sign out',
              onPressed: widget.authVm.signOut,
            ),
          ],
        ),
        body: Column(
          children: [
            Padding(
              padding: const EdgeInsets.all(12),
              child: Row(
                children: [
                  Expanded(
                    child: TextField(
                      key: const Key('todos.input'),
                      controller: _newTodo,
                      decoration: const InputDecoration(
                        hintText: 'Add a todo',
                        border: OutlineInputBorder(),
                      ),
                      onSubmitted: (_) => _add(),
                    ),
                  ),
                  const SizedBox(width: 8),
                  IconButton(
                    key: const Key('todos.add'),
                    icon: const Icon(Icons.add),
                    onPressed: _add,
                  ),
                ],
              ),
            ),
            Expanded(
              child: ListenableBuilder(
                listenable: widget.todoVm,
                builder: (context, _) => ListView(
                  key: const Key('todos.list'),
                  children: [
                    for (final t in widget.todoVm.todos)
                      CheckboxListTile(
                        key: ValueKey(t.id),
                        value: t.done,
                        onChanged: (_) => widget.todoVm.toggle(t.id),
                        title: Text(t.title),
                      ),
                  ],
                ),
              ),
            ),
          ],
        ),
      );
}
