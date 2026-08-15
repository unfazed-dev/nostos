import 'dart:async';
import 'dart:io';

import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import 'adapters/nostos_adapter.dart';
import 'connectivity_guard.dart';
import 'adapters/powersync_adapter.dart';
import 'adapters/sync_adapter.dart';
import 'bench/harness.dart';
import 'bench/store.dart';
import 'bench/upload.dart';
import 'design/tokens.dart';
import 'engine_registry.dart';
import 'push/push_pilot.dart';
import 'ui/analytics.dart';
import 'ui/connectivity_led.dart';
import 'ui/home.dart';
import 'ui/shop.dart';
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

// ponytail: no package_info_plus dep for one hand-copied version string;
// wire it in if the bench harness ever needs per-build accuracy.
const _appVersion = '1.0.0+1'; // mirrors pubspec.yaml's `version:`

// PILOT (ADR-0037): opt-in FCM doorbell wiring — see lib/push/push_pilot.dart.
// Off by default so builds/analyze/tests stay green without operator-owned
// Firebase config (google-services.json / GoogleService-Info.plist).
const _pushPilotEnabled = bool.fromEnvironment('ATLET_PUSH_PILOT');

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await Supabase.initialize(
    url: _supabaseUrl,
    publishableKey: _supabaseAnonKey,
  );
  if (_pushPilotEnabled) {
    // Platform config (google-services.json / GoogleService-Info.plist) —
    // throws here when absent, which is the point of the opt-in flag.
    await Firebase.initializeApp();
    FirebaseMessaging.onBackgroundMessage(nostosDoorbellBackgroundHandler);
  }
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
          onSignedIn: () => Navigator.of(context).pushReplacementNamed('/home'),
        ),
        '/home': (context) => const HomeScreen(),
      },
    );
  }
}

/// Home shell: bottom-nav host for Home / Shop / Analytics (I-1 fix —
/// final-review-verdict.md). Home hosts the training UI (T12) and the
/// settings-sheet entry point for the engine toggle (T11); Shop and
/// Analytics are the other two tabs, built lazily so this screen stays
/// constructible with no live Supabase session (see widget_test.dart).
class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, this.benchStoreOpener});

  /// Injectable so widget tests never need a live path_provider platform
  /// channel to reach the Analytics tab — mirrors [AnalyticsScreen]'s own
  /// store/runSuite/uploadRuns injection (ui/analytics.dart). Defaults to
  /// the real app-documents JSONL store in production.
  final Future<BenchStore> Function()? benchStoreOpener;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  int _tabIndex = 0;
  Future<BenchStore>? _benchStoreFuture;
  ConnectivityGuard? _connectivityGuard;

  /// Drives the offline banner. Sourced from platform connectivity (the
  /// guard), not the engine's `connected` stream: the banner must show even
  /// when no engine is live (signed out) and must not flicker on the
  /// engine's internal reconnect cycles.

  Future<BenchStore> _benchStore() => _benchStoreFuture ??=
      (widget.benchStoreOpener ?? BenchStore.openAppDocuments)();

  @override
  void initState() {
    super.initState();
    // Nostos is the default engine: bring it up automatically on entering
    // Home so the user never has to open Settings to start syncing —
    // Settings is only for *switching* engines. Post-frame so nothing
    // touches `Supabase.instance` during initState (widget_test.dart).
    // The connectivity guard also starts post-frame: platform channels are
    // unavailable during widget-test initState, and start() is what opens
    // the connectivity_plus stream.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _autoStartDefaultEngine();
      _startConnectivityGuard();
    });
  }

  /// Instant offline/online reaction (loss → clean disconnect so the UI
  /// flips offline immediately; regain → resume(), which short-circuits
  /// reconnect backoff and replays the outbox). Without this the engine only
  /// notices a silently-dead socket via its 30s idle backstop.
  void _startConnectivityGuard() {
    if (_connectivityGuard != null) return;
    _connectivityGuard = ConnectivityGuard(
      onOnlineChanged: (online) async {
        connectivityOnline.value = online; // drives the AppBar LED reactively
        final adapter = engineRegistry.current;
        if (adapter == null) return; // signed out / no engine live
        try {
          await adapter.setConnected(online);
        } catch (e) {
          // Never crash on a connectivity flap mid engine-switch; the engine's
          // own backoff still covers reconnects if this call loses the race.
          debugPrint('connectivity guard: setConnected($online) failed: $e');
        }
      },
    )..start();
  }

  @override
  void dispose() {
    _connectivityGuard?.dispose();
    _connectivityGuard = null;
    super.dispose();
  }

  /// Starts [Engine.cairn] if no engine is live yet and a Supabase session
  /// exists. Silently a no-op in widget tests (no `Supabase.initialize()`)
  /// and when an engine is already live (hot reload, route re-push).
  Future<void> _autoStartDefaultEngine() async {
    if (engineRegistry.activeEngine != null) return;
    try {
      if (Supabase.instance.client.auth.currentSession == null) return;
    } catch (_) {
      return; // Supabase not initialized (widget tests) — stay engine-less.
    }
    await _switchEngine(Engine.cairn);
  }

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
      final adapter = await engineRegistry.switchTo(
        target,
        SyncSession(
          supabaseUrl: _supabaseUrl,
          accessToken: session.accessToken,
          userId: session.user.id,
          dbDir: dbDir,
        ),
      );
      // PILOT (ADR-0037): doorbell registration follows the nostos engine —
      // push is a nostos feature, PowerSync has no rail. detach() on switch-
      // away only unwires handlers; the SDK's sign-out hook (run inside the
      // registry's wipe, above) deregisters the tokens.
      if (_pushPilotEnabled) {
        if (target == Engine.cairn) {
          unawaited(pushPilot.attach(adapter as NostosAdapter));
        } else {
          unawaited(pushPilot.detach());
        }
      }
      _notify('Now syncing with ${target.name}.');
    } catch (e) {
      _notify('Engine switch failed: $e');
    }
    if (mounted) setState(() {}); // refresh the settings sheet's selection
  }

  void _notify(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(
      context,
    ).showSnackBar(SnackBar(content: Text(message)));
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

  /// Builds the Nth synthetic bench session — mirrors test/harness_test.dart's
  /// `_buildSession` fixture. `id` is unused: addSession()/PostgREST both
  /// assign their own ids (see runner.dart's writeAck/queueDrain).
  SessionRow _benchSessionRow(int i) => SessionRow(
    id: 'unused',
    title: 'Bench $i',
    // Must satisfy 0001_atlet_schema.sql's CHECKs: type in
    // ('distance','reps','time'), unit in ('km','reps','sec') — 'run'/'m'
    // was rejected by sessions_type_check and failed every bench insert.
    type: 'distance',
    metric: 5 + i,
    unit: 'km',
    occurredOn: DateTime.now().toUtc(),
  );

  /// Production wiring for [AnalyticsScreen.runSuite]: runs the plan's
  /// two-engine comparison (bench/harness.dart's `runFullSuiteForBothEngines`)
  /// against fresh Nostos/PowerSync adapters. Deliberately bypasses
  /// [engineRegistry] — see that function's own doc comment on why a bench
  /// run's needs (signOut after every suite, two live dbDirs) don't fit the
  /// registry's single-slot contract (decision #4).
  Future<void> _runBenchSuite(BenchStore store) async {
    final client = Supabase.instance.client;
    final session = client.auth.currentSession;
    if (session == null) {
      throw StateError('No active session — sign in again.');
    }
    final baseDir = (await getApplicationDocumentsDirectory()).path;
    // coldSync gates completion on `rows.length == seedSize` (runner.dart) —
    // has to be the real current row count, not a guess.
    final existingSessions = await client.from('sessions').select('id');
    await runFullSuiteForBothEngines(
      sdk: 'flutter',
      specVersion: 'v0',
      seedSize: existingSessions.length,
      appVersion: _appVersion,
      device: {'model': 'flutter-app', 'os': Platform.operatingSystem},
      rootDbDir: '$baseDir/atlet_bench',
      supabaseUrl: _supabaseUrl,
      accessToken: session.accessToken,
      userId: session.user.id,
      store: store,
      insertRemoteRow: supabasePostgrestInsert(
        client,
        buildRow: _benchSessionRow,
      ),
      buildSession: _benchSessionRow,
      adapterFactories: {
        Engine.cairn: () => NostosAdapter(),
        Engine.powersync: () => PowerSyncAdapter(),
      },
    );
  }

  Widget _buildHomeTab(BuildContext context) {
    return Scaffold(
      backgroundColor: AtletTokens.bone,
      appBar: AppBar(
        backgroundColor: AtletTokens.bone,
        elevation: 0,
        title: Text('Home', style: TextStyle(color: AtletTokens.ink)),
        actions: [
          const ConnectivityLed(),
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

  Widget _buildAnalyticsTab(BuildContext context) {
    return FutureBuilder<BenchStore>(
      future: _benchStore(),
      builder: (context, snapshot) {
        final store = snapshot.data;
        if (store == null) {
          return const Scaffold(
            backgroundColor: AtletTokens.bone,
            body: Center(
              child: CircularProgressIndicator(color: AtletTokens.accent),
            ),
          );
        }
        return AnalyticsScreen(
          store: store,
          // Lazy: Supabase.instance.client is touched only when Upload is
          // actually tapped, not merely when this tab is built — keeps the
          // tab reachable in widget tests with no Supabase.initialize().
          uploadRuns: (rows) =>
              supabasePostgrestUpload(Supabase.instance.client)(rows),
          runSuite: () => _runBenchSuite(store),
        );
      },
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      key: const Key('home-shell'),
      body: Column(
        children: [
          // Offline banner removed — the AppBar ConnectivityLed carries the
          // online/offline signal now (user request 2026-08-07).
          Expanded(
            child: switch (_tabIndex) {
              0 => _buildHomeTab(context),
              1 => ShopScreen(adapter: engineRegistry.current),
              _ => _buildAnalyticsTab(context),
            },
          ),
        ],
      ),
      bottomNavigationBar: NavigationBar(
        key: const Key('main-nav-bar'),
        selectedIndex: _tabIndex,
        backgroundColor: AtletTokens.bone,
        onDestinationSelected: (index) => setState(() => _tabIndex = index),
        destinations: const [
          NavigationDestination(
            key: Key('nav-tab-home'),
            icon: Icon(Icons.home_outlined),
            selectedIcon: Icon(Icons.home),
            label: 'Home',
          ),
          NavigationDestination(
            key: Key('nav-tab-shop'),
            icon: Icon(Icons.storefront_outlined),
            selectedIcon: Icon(Icons.storefront),
            label: 'Shop',
          ),
          NavigationDestination(
            key: Key('nav-tab-analytics'),
            icon: Icon(Icons.analytics_outlined),
            selectedIcon: Icon(Icons.analytics),
            label: 'Analytics',
          ),
        ],
      ),
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
