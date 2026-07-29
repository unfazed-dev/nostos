import 'package:nostos_flutter/nostos_flutter.dart' show NostosConnectionState, SyncStatus;
import 'package:flutter/material.dart';

import '../domain/todo_repository.dart';
import '../viewmodels/auth_viewmodel.dart';
import '../viewmodels/todo_viewmodel.dart';

/// Todo home: a new-todo [TextField] (`todos.input`) + add [IconButton]
/// (`todos.add`) + a [ListView] (`todos.list`) of todos + a sign-out
/// [IconButton] (`auth.signout`).
///
/// Each row supports three actions: toggle done ([Checkbox]), edit title
/// (trailing [IconButton] → [AlertDialog]), and swipe-to-delete
/// ([DismissDirection.endToStart] → [TodoViewModel.remove]). A connectivity
/// banner ([_SyncStatusBanner]) sits below the AppBar.
///
/// The view is always constructed with parent-owned VMs ([authVm] + [todoVm]);
/// it never disposes them — [TodoApp] owns their lifecycle (calling
/// [TodoViewModel.dispose] here would double-dispose).
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

  Future<void> _editTitle(Todo t) async {
    final controller = TextEditingController(text: t.title);
    final next = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Edit todo'),
        content: TextField(
          controller: controller,
          autofocus: true,
          decoration: const InputDecoration(
            hintText: 'Title',
            border: OutlineInputBorder(),
          ),
          onSubmitted: (value) => Navigator.of(ctx).pop(value.trim()),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(),
            child: const Text('Cancel'),
          ),
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(controller.text.trim()),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    controller.dispose();
    if (next == null || next.isEmpty || next == t.title) return;
    await widget.todoVm.update(t.id, title: next);
  }

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
            _SyncStatusBanner(todoVm: widget.todoVm),
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
                      Dismissible(
                        key: ValueKey(t.id),
                        direction: DismissDirection.endToStart,
                        background: Container(
                          color: Colors.red,
                          alignment: Alignment.centerRight,
                          padding: const EdgeInsets.only(right: 16),
                          child: const Icon(Icons.delete, color: Colors.white),
                        ),
                        // Await remove before the row animates away so a
                        // failed delete keeps the row in place.
                        confirmDismiss: (_) async {
                          await widget.todoVm.remove(t.id);
                          return true;
                        },
                        child: ListTile(
                          leading: Checkbox(
                            value: t.done,
                            onChanged: (_) => widget.todoVm.toggle(t.id),
                          ),
                          title: Text(t.title),
                          trailing: IconButton(
                            key: ValueKey('${t.id}-edit'),
                            icon: const Icon(Icons.edit),
                            tooltip: 'Edit title',
                            onPressed: () => _editTitle(t),
                          ),
                        ),
                      ),
                  ],
                ),
              ),
            ),
          ],
        ),
      );
}

/// ADR-0024: surfaces [TodoViewModel.currentStatus] (a [SyncStatus] from the
/// reactive `Collection<T>` facade) as a one-line connectivity banner. Hidden
/// when the backend has no notion of connectivity (mock + Supabase-direct →
/// [TodoViewModel.currentStatus] is null). Rebuilds off the view-model's
/// `notifyListeners` (the VM wraps the underlying `ValueListenable<SyncStatus>`
/// and re-emits on change — see _TodoViewModel._onStatus).
class _SyncStatusBanner extends StatelessWidget {
  const _SyncStatusBanner({required this.todoVm});

  final TodoViewModel todoVm;

  @override
  Widget build(BuildContext context) => ListenableBuilder(
        listenable: todoVm,
        builder: (context, _) {
          final status = todoVm.currentStatus;
          if (status == null) return const SizedBox.shrink();
          final offline = !status.connected;
          return Material(
            color: offline ? Colors.orange.shade50 : Colors.green.shade50,
            child: Padding(
              padding:
                  const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
              child: Row(
                children: [
                  Icon(
                    offline ? Icons.cloud_off : Icons.cloud_done,
                    size: 16,
                    color: offline
                        ? Colors.orange.shade700
                        : Colors.green.shade700,
                  ),
                  const SizedBox(width: 6),
                  Text(
                    _label(status),
                    style: Theme.of(context).textTheme.bodySmall,
                  ),
                ],
              ),
            ),
          );
        },
      );

  static String _label(SyncStatus status) {
    if (status.connected) {
      final lastSyncedAt = status.lastSyncedAt;
      if (lastSyncedAt == null) return 'synced';
      final delta = DateTime.now().difference(lastSyncedAt);
      final secs = delta.inSeconds;
      if (secs < 5) return 'synced just now';
      if (secs < 60) return 'synced ${secs}s ago';
      if (delta.inMinutes < 60) return 'synced ${delta.inMinutes}m ago';
      return 'synced ${delta.inHours}h ago';
    }
    final conn = status.conn;
    if (conn == NostosConnectionState.connecting ||
        conn == NostosConnectionState.reconnecting) {
      return 'connecting…';
    }
    return 'offline';
  }
}
