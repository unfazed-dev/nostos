import 'package:flutter/material.dart';
import 'package:supabase_flutter/supabase_flutter.dart' hide Session, AuthException;

import 'domain/auth_gateway.dart';
import 'domain/todo_repository.dart';
import 'env.dart';
import 'infra/nostos_todo_repository.dart';
import 'infra/fake_auth_gateway.dart';
import 'infra/in_memory_todo_repository.dart';
import 'infra/supabase_auth_gateway.dart';
import 'infra/supabase_todo_repository.dart';
import 'views/app.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  late final AuthGateway auth;
  late final TodoRepository todos;
  if (Env.isNostosLive) {
    // W5 showcase: real nostos-server + real docker Postgres + a pre-minted
    // JWT (no Supabase project exists yet — W0b is operator-blocked). Which
    // "user" this app instance is comes entirely from the JWT baked into
    // Env.nostosToken at launch (tool/mint_jwt.sh), not from the sign-in
    // screen below — FakeAuthGateway only gates the UI, matching mock mode.
    auth = FakeAuthGateway();
    todos = await NostosTodoRepository.connect(
        wsUrl: Env.nostosWsUrl, token: Env.nostosToken);
  } else if (Env.isLive) {
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
