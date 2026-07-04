import 'package:flutter/material.dart';
import 'package:supabase_flutter/supabase_flutter.dart' hide Session, AuthException;

import 'domain/auth_gateway.dart';
import 'domain/todo_repository.dart';
import 'env.dart';
import 'infra/fake_auth_gateway.dart';
import 'infra/in_memory_todo_repository.dart';
import 'infra/supabase_auth_gateway.dart';
import 'infra/supabase_todo_repository.dart';
import 'views/app.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  late final AuthGateway auth;
  late final TodoRepository todos;
  if (Env.isLive) {
    await Supabase.initialize(
        url: Env.supabaseUrl, publishableKey: Env.supabaseAnonKey);
    final client = Supabase.instance.client;
    auth = SupabaseAuthGateway(client);
    todos = SupabaseTodoRepository(client);
  } else {
    auth = FakeAuthGateway();
    todos = InMemoryTodoRepository();
  }
  runApp(TodoApp(auth: auth, todos: todos));
}
