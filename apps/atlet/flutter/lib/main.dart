import 'package:flutter/material.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import 'design/tokens.dart';
import 'ui/signin.dart';

// ponytail: real values are operator-owned (apps/atlet/services/.env.example
// has no anon key checked in, by design). These compile-time defaults are
// obviously-placeholder and only support a build/analyze/boot-level check;
// a live sign-in needs `--dart-define=SUPABASE_URL=... --dart-define=SUPABASE_ANON_KEY=...`.
const _supabaseUrl = String.fromEnvironment(
  'SUPABASE_URL',
  defaultValue: 'https://PROJECT_REF.supabase.co',
);
const _supabaseAnonKey = String.fromEnvironment(
  'SUPABASE_ANON_KEY',
  defaultValue: 'PLACEHOLDER_ANON_KEY',
);

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await Supabase.initialize(url: _supabaseUrl, publishableKey: _supabaseAnonKey);
  runApp(const AtletApp());
}

class AtletApp extends StatelessWidget {
  const AtletApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Atlet',
      theme: ThemeData(
        useMaterial3: true,
        scaffoldBackgroundColor: AtletTokens.paper,
        colorScheme: ColorScheme.fromSeed(
          seedColor: AtletTokens.accent,
          surface: AtletTokens.paper,
        ),
        fontFamily: AtletTokens.sansFamily,
      ),
      initialRoute: '/signin',
      routes: {
        '/signin': (context) => SigninScreen(
              onSignedIn: () =>
                  Navigator.of(context).pushReplacementNamed('/home'),
            ),
        '/home': (context) => const HomeScreen(),
      },
    );
  }
}

/// Minimal home shell — proves the post-signin route exists. Session/product
/// views land in later Atlet tasks (T7+).
class HomeScreen extends StatelessWidget {
  const HomeScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: AtletTokens.bone,
      appBar: AppBar(
        backgroundColor: AtletTokens.bone,
        elevation: 0,
        title: Text('Atlet', style: TextStyle(color: AtletTokens.ink)),
      ),
      body: Center(
        child: Text(
          'Home',
          style: TextStyle(
            fontSize: AtletTokens.largeTitle,
            fontWeight: FontWeight.w600,
            color: AtletTokens.ink,
          ),
        ),
      ),
    );
  }
}
