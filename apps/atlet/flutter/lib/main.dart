import 'dart:async';

import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import 'design/tokens.dart';
import 'engine_registry.dart';
import 'ui/home.dart';
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

/// Single registry for the app's lifetime. Owns which sync engine is live
/// and enforces plan decision #4 (never both engines live at once) — see
/// lib/engine_registry.dart. Module-level so it survives HomeScreen
/// rebuilds/route pushes without needing an InheritedWidget for this pilot.
final EngineRegistry engineRegistry = EngineRegistry();

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

/// Home shell: owns the settings-sheet entry point for the engine toggle
/// (T11) and hosts the training UI (T12), which renders exclusively from
/// the active adapter's watchSessions() stream — see ui/home.dart.
class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key});

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  /// Switches the live sync engine to [target] via [engineRegistry], which
  /// wipes the outgoing adapter (if any) before bringing the new one up
  /// (decision #4). Reads the current Supabase session lazily, on tap, so
  /// this never touches `Supabase.instance` during build/initState — that
  /// keeps HomeScreen constructible in widget tests that don't call
  /// `Supabase.initialize()` (see widget_test.dart).
  Future<void> _switchEngine(Engine target) async {
    if (engineRegistry.activeEngine == target) return;
    final session = Supabase.instance.client.auth.currentSession;
    if (session == null) {
      _notify('No active session — sign in again.');
      return;
    }
    _notify('Switching to ${target.name}…');
    try {
      final dbDir = (await getApplicationDocumentsDirectory()).path;
      await engineRegistry.switchTo(
        target,
        SyncSession(
          supabaseUrl: _supabaseUrl,
          accessToken: session.accessToken,
          userId: session.user.id,
          dbDir: dbDir,
        ),
      );
      _notify('Now syncing with ${target.name}.');
    } catch (e) {
      _notify('Engine switch failed: $e');
    }
    if (mounted) setState(() {}); // refresh the settings sheet's selection
  }

  void _notify(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
        .showSnackBar(SnackBar(content: Text(message)));
  }

  void _openSettings() {
    showModalBottomSheet<void>(
      context: context,
      builder: (_) => _EngineSettingsSheet(
        activeEngine: engineRegistry.activeEngine,
        onSelect: (engine) {
          Navigator.of(context).pop();
          unawaited(_switchEngine(engine));
        },
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: AtletTokens.bone,
      appBar: AppBar(
        backgroundColor: AtletTokens.bone,
        elevation: 0,
        title: Text('Home', style: TextStyle(color: AtletTokens.ink)),
        actions: [
          IconButton(
            key: const Key('settings-button'),
            icon: const Icon(Icons.settings_outlined),
            tooltip: 'Settings',
            onPressed: _openSettings,
          ),
        ],
      ),
      body: TrainingHome(adapter: engineRegistry.current),
    );
  }
}

/// Settings-sheet content: the sync-engine toggle. Selecting an engine that
/// isn't already active runs [EngineRegistry.switchTo] via [onSelect] — a
/// full wipe of the outgoing adapter (if any) before the incoming one is
/// constructed and init()'d (decision #4: never both engines live at once).
class _EngineSettingsSheet extends StatelessWidget {
  const _EngineSettingsSheet({
    required this.activeEngine,
    required this.onSelect,
  });

  final Engine? activeEngine;
  final ValueChanged<Engine> onSelect;

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(
              'SYNC ENGINE',
              style: TextStyle(
                fontSize: AtletTokens.footnote,
                letterSpacing: 1.5,
                color: AtletTokens.ink3,
                fontWeight: FontWeight.w500,
              ),
            ),
            const SizedBox(height: 16),
            RadioGroup<Engine>(
              groupValue: activeEngine,
              onChanged: (value) {
                if (value != null) onSelect(value);
              },
              child: Column(
                children: [
                  for (final engine in Engine.values)
                    RadioListTile<Engine>(
                      key: Key('engine-option-${engine.name}'),
                      title: Text(engine.name),
                      value: engine,
                    ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
