import 'package:flutter/material.dart';

import '../domain/auth_gateway.dart';
import '../domain/todo_repository.dart';
import '../viewmodels/auth_viewmodel.dart';
import '../viewmodels/todo_viewmodel.dart';
import 'sign_in_view.dart';
import 'todo_view.dart';

/// Root widget. Owns the [AuthViewModel] + [TodoViewModel] and swaps
/// [SignInView]↔[TodoView] on the auth VM's session. Constructed by
/// `main.dart` with the selected ports (fakes in mock mode, Supabase adapters
/// in live mode — see [Env.isLive]).
class TodoApp extends StatefulWidget {
  const TodoApp({super.key, required this.auth, required this.todos});

  final AuthGateway auth;
  final TodoRepository todos;

  @override
  State<TodoApp> createState() => _TodoAppState();
}

class _TodoAppState extends State<TodoApp> {
  late final AuthViewModel _authVm = AuthViewModel(widget.auth);
  late final TodoViewModel _todoVm = TodoViewModel(widget.todos);

  @override
  void dispose() {
    _authVm.dispose();
    _todoVm.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Todos',
        theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
        home: ListenableBuilder(
          listenable: _authVm,
          builder: (context, _) => _authVm.session == null
              ? SignInView(auth: widget.auth, viewModel: _authVm)
              : TodoView(authVm: _authVm, todoVm: _todoVm),
        ),
      );
}
