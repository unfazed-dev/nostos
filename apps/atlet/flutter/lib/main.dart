import 'dart:async';
import 'dart:io';

import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/material.dart';
import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:flutter/services.dart';
import 'package:path_provider/path_provider.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import 'adapters/nostos_adapter.dart';
import 'connectivity_guard.dart';
import 'adapters/sync_adapter.dart';
import 'bench/harness.dart';
import 'bench/store.dart';
import 'bench/upload.dart';
import 'design/tokens.dart';
import 'engine_registry.dart';
import 'push/order_push.dart';
import 'push/push_pilot.dart';
import 'ui/history.dart';
import 'ui/history_detail.dart';
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
// NOTE: bool.fromEnvironment only accepts the literal string "true" — pass
// `--dart-define=ATLET_PUSH_PILOT=true`; `=1` silently parses as false.
const _pushPilotEnabled = bool.fromEnvironment('ATLET_PUSH_PILOT');

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await Supabase.initialize(
    url: _supabaseUrl,
    publishableKey: _supabaseAnonKey,
  );
  // Firebase is the mobile rail only. On web the pilot uses raw Web Push
  // (push_pilot_web.dart) — no Firebase init, no web config to throw on.
  if (_pushPilotEnabled && !kIsWeb) {
    // Platform config (google-services.json / GoogleService-Info.plist) —
    // throws here when absent, which is the point of the opt-in flag.
    await Firebase.initializeApp();
    FirebaseMessaging.onBackgroundMessage(nostosDoorbellBackgroundHandler);
    _wireFcmTaps();
  }
  // Local banners exist with or without the FCM pilot, so their tap path does
  // too. Web has no MethodChannel — its banner is a snackbar, already on-screen.
  if (!kIsWeb) _wireNotificationTaps();
  runApp(const AtletApp());
}

/// Foreground order-banner bridge: MainActivity/AppDelegate post a local
/// heads-up on the same 'nostos' channel as the FCM pushes (see push pilot,
/// ADR-0037), and hand the tap back over the same channel.
const _orderBannerChannel = MethodChannel('atlet/notify');

/// The navigator a notification tap pushes onto. A tap arrives from the
/// platform, not from a widget, so there is no BuildContext to route with.
final GlobalKey<NavigatorState> navigatorKey = GlobalKey<NavigatorState>();

/// A tap that landed before the navigator existed (cold start: the OS reports
/// the tap while Dart is still booting). Flushed on HomeScreen's first frame.
String? _pendingEventId;

/// Route currently open, so the same event cannot be pushed twice — a
/// terminated-state launch is reported by BOTH the platform tap buffer and
/// FirebaseMessaging.getInitialMessage().
String? _openEventRoute;

/// An empty history that more than one widget can listen to. `Stream.value`
/// is single-subscription: the feed listens first, and the detail screen's
/// listen on the same object throws. The engine's own streams are already
/// `Stream.multi` with replay (replayLatest), so this only covers the
/// no-engine case.
Stream<List<OrderEventRow>> _noOrderEvents() =>
    Stream<List<OrderEventRow>>.multi((c) => c.add(const []));

/// The one destination of every deep link, whatever carried it: the History
/// detail for one order event.
void openHistoryEvent(String eventId) {
  final route = historyRoute(eventId);
  final nav = navigatorKey.currentState;
  if (nav == null) {
    _pendingEventId = eventId;
    return;
  }
  if (_openEventRoute == route) return;
  _openEventRoute = route;
  debugPrint('notification tap: $route'); // tool/atlet_watch.sh greps this
  nav
      .push(
        MaterialPageRoute<void>(
          settings: RouteSettings(name: route),
          builder: (_) => HistoryDetailScreen(
            eventId: eventId,
            events:
                engineRegistry.current?.watchOrderEvents() ?? _noOrderEvents(),
          ),
        ),
      )
      .whenComplete(() {
        if (_openEventRoute == route) _openEventRoute = null;
      });
}

/// Taps that come through the platform: a local banner opened, or an
/// `atlet://history/<id>` URL (AppDelegate forwards both as `notification_tap`).
void _wireNotificationTaps() {
  _orderBannerChannel.setMethodCallHandler((call) async {
    if (call.method != 'notification_tap') return null;
    final args = call.arguments;
    if (args is Map) {
      final id = tappedEventId(args);
      if (id != null) openHistoryEvent(id);
    }
    return null;
  });
  // Cold start: the platform may have delivered the tap before Dart had a
  // handler to receive it, so ask rather than wait to be told.
  unawaited(
    _orderBannerChannel
        .invokeMapMethod<Object?, Object?>('take_pending_tap')
        .then((tap) {
          if (tap == null) return;
          final id = tappedEventId(tap);
          if (id != null) openHistoryEvent(id);
        })
        // No platform side (web, tests) means no buffered tap — not an error.
        .catchError((Object _) {}),
  );
}

/// Taps on a REAL push. `message.data` is where the routing keys live —
/// see orderPushPayload, and Firebase's own "handle interaction" guidance:
/// https://firebase.google.com/docs/cloud-messaging/flutter/receive-messages
void _wireFcmTaps() {
  void open(RemoteMessage message) {
    final id = tappedEventId(message.data);
    if (id == null) return; // someone else's push
    recordPushAttempt(
      PushAttempt(
        eventId: id,
        at: DateTime.now(),
        channel: 'fcm-open',
        payload: message.data,
      ),
    );
    openHistoryEvent(id);
  }

  // Terminated (the push launched the app) and background, respectively —
  // both are needed; neither covers the other.
  unawaited(
    FirebaseMessaging.instance.getInitialMessage().then((m) {
      if (m != null) open(m);
    }),
  );
  FirebaseMessaging.onMessageOpenedApp.listen(open);
}

/// Single registry for the app's lifetime. Owns which sync engine is live
/// and enforces plan decision #4 (never two engines live at once) — see
/// lib/engine_registry.dart. Module-level so it survives HomeScreen
/// rebuilds/route pushes without needing an InheritedWidget for this pilot.
final EngineRegistry engineRegistry = EngineRegistry(
  supabaseAnonKey: _supabaseAnonKey,
);

class AtletApp extends StatelessWidget {
  const AtletApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Atlet',
      navigatorKey: navigatorKey,
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

/// Home shell: bottom-nav host for Home / Shop / History (I-1 fix —
/// final-review-verdict.md). Home hosts the training UI (T12); Shop and
/// History are the other two tabs, built lazily so this screen stays
/// constructible with no live Supabase session (see widget_test.dart).
class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, this.benchStoreOpener});

  /// Injectable so widget tests never need a live path_provider platform
  /// channel to reach the History tab — mirrors [HistoryScreen]'s own
  /// store/runSuite/uploadRuns injection (ui/history.dart). Defaults to
  /// the real app-documents JSONL store in production.
  final Future<BenchStore> Function()? benchStoreOpener;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  int _tabIndex = 0;
  Future<BenchStore>? _benchStoreFuture;
  ConnectivityGuard? _connectivityGuard;

  // PILOT (ADR-0037): foreground order-status banner — the online half of the
  // push story. While the app is connected, the vendor's status UPDATE
  // arrives over the live sync socket (a push is suppressed by the offline
  // gate by design), so the in-app banner IS the foreground notification.
  StreamSubscription<List<OrderEventRow>>? _orderBannerSub;

  /// Every event id this run has already seen. The first emission only seeds
  /// it — the history a fresh device pulls is not news.
  final Set<String> _seenEventIds = {};
  bool _eventsSeeded = false;

  /// Forwards rotated Supabase JWTs into the live engine. Without it the
  /// engine keeps the token it was opened with until it expires — see
  /// NostosAdapter.setToken for what that costs.
  StreamSubscription<AuthState>? _authSub;

  /// Drives the offline banner. Sourced from platform connectivity (the
  /// guard), not the engine's `connected` stream: the banner must show even
  /// when no engine is live (signed out) and must not flicker on the
  /// engine's internal reconnect cycles.

  Future<BenchStore> _benchStore() => _benchStoreFuture ??=
      (widget.benchStoreOpener ?? BenchStore.openAppDocuments)();

  @override
  void initState() {
    super.initState();
    // Direct-mode Nostos is the only engine: bring it up on entering Home so
    // syncing is live the moment the app is. Post-frame so nothing touches
    // `Supabase.instance` during initState (widget_test.dart).
    // The connectivity guard also starts post-frame: platform channels are
    // unavailable during widget-test initState, and start() is what opens
    // the connectivity_plus stream.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _startEngine();
      _startConnectivityGuard();
      // A notification tapped from a cold start reaches openHistoryEvent
      // before any navigator exists; this is the first frame that has one.
      final pending = _pendingEventId;
      if (pending != null) {
        _pendingEventId = null;
        openHistoryEvent(pending);
      }
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
    _orderBannerSub?.cancel();
    _orderBannerSub = null;
    _authSub?.cancel();
    _authSub = null;
    super.dispose();
  }

  /// Foreground half of the push pilot: a banner per order event seen on the
  /// live `watchOrderEvents()` stream (migration 0007), which is also the row
  /// the History tab shows and the id the notification deep-links to — one
  /// source, so a tapped banner and a tapped row cannot disagree.
  ///
  /// The first emission only seeds `_seenEventIds`: the backfilled history a
  /// device pulls on open is not news, and neither is the user's own checkout.
  void _wireOrderBanner(NostosAdapter adapter) {
    _orderBannerSub?.cancel();
    _seenEventIds.clear();
    _eventsSeeded = false;
    _orderBannerSub = adapter.watchOrderEvents().listen((events) {
      final fresh = events.where((e) => !_seenEventIds.contains(e.id)).toList();
      _seenEventIds.addAll(events.map((e) => e.id));
      if (!_eventsSeeded) {
        _eventsSeeded = true;
        return;
      }
      // The stream is newest-first; post oldest-first so a burst reads in the
      // order it happened.
      for (final e in fresh.reversed) {
        unawaited(_postOrderBanner(e));
      }
    });
  }

  /// Posts one order event to the user and records what the platform did with
  /// it. Logged, not just posted: a banner that never appears and a banner
  /// never asked for look identical from outside the app, and
  /// tool/atlet_watch.sh greps for exactly this line.
  Future<void> _postOrderBanner(OrderEventRow e) async {
    final payload = orderPushPayload(e);
    debugPrint(
      'order banner: ${e.orderId.substring(0, 8)} '
      '${e.previousStatus} -> ${e.status} route=${historyRoute(e.id)}',
    );
    if (!postsOwnBanner(pushPilot: _pushPilotEnabled, web: kIsWeb)) return;
    // Web has no MethodChannel: the snackbar IS the foreground banner.
    if (kIsWeb) {
      _notify(payload['body']! as String);
      recordPushAttempt(
        PushAttempt(
          eventId: e.id,
          at: DateTime.now(),
          channel: 'snackbar',
          payload: payload,
        ),
      );
      return;
    }
    String? error;
    try {
      await _orderBannerChannel.invokeMethod('order_update', payload);
    } catch (err) {
      error = '$err';
      debugPrint('order banner failed: $err');
    }
    recordPushAttempt(
      PushAttempt(
        eventId: e.id,
        at: DateTime.now(),
        channel: 'local-banner',
        payload: payload,
        error: error,
      ),
    );
  }

  /// Brings the sync engine up. There is one engine and no way to change it
  /// (user request 2026-09-22): direct-mode Nostos, the device syncing with
  /// Supabase itself with no `nostos-server` on the other end (ADR-0045).
  /// It starts on its own when Home opens, and says nothing while doing it —
  /// the connectivity LED and the write-status UI are what report a sync
  /// that isn't working.
  ///
  /// Reads the Supabase session lazily, post-frame, so this never touches
  /// `Supabase.instance` during build/initState — that keeps HomeScreen
  /// constructible in widget tests that don't call `Supabase.initialize()`
  /// (see widget_test.dart). No-op when an engine is already live (hot
  /// reload, route re-push) or when there is no session.
  Future<void> _startEngine() async {
    if (engineRegistry.activeEngine != null) return;
    Session? session;
    try {
      session = Supabase.instance.client.auth.currentSession;
    } catch (_) {
      return; // Supabase not initialized (widget tests) — stay engine-less.
    }
    if (session == null) return; // signed out — sign-in re-enters Home
    try {
      // path_provider has no web impl; the web engine's storage is
      // OPFS-backed and ignores sqlitePath (ADR-0036), so dbDir is an
      // unused placeholder there.
      final dbDir = kIsWeb
          ? ''
          : (await getApplicationDocumentsDirectory()).path;
      final adapter = await engineRegistry.start(
        Engine.nostosDirect,
        SyncSession(
          supabaseUrl: _supabaseUrl,
          accessToken: session.accessToken,
          userId: session.user.id,
          dbDir: dbDir,
        ),
      );
      final nostos = adapter as NostosAdapter;
      // The order banner is pure sync — it reads watchOrders() and posts a
      // local notification. It was gated behind the FCM pilot, which meant a
      // default build showed the user nothing when their order shipped.
      _wireOrderBanner(nostos);
      // Supabase rotates the access token about hourly. The engine was opened
      // with one token and has no way to learn the next, so forward it.
      await _authSub?.cancel();
      _authSub = Supabase.instance.client.auth.onAuthStateChange.listen((s) {
        final token = s.session?.accessToken;
        if (token == null) return;
        if (s.event == AuthChangeEvent.tokenRefreshed ||
            s.event == AuthChangeEvent.signedIn) {
          // Best-effort: a refused swap leaves the previous token in place and
          // the next rotation tries again.
          unawaited(nostos.setToken(token).catchError((Object _) {}));
        }
      });
      // PILOT (ADR-0037): doorbell registration follows the nostos engine —
      // push is a nostos feature, and direct mode is still a NostosAdapter.
      if (_pushPilotEnabled) {
        unawaited(pushPilot.attach(nostos));
      }
    } catch (e) {
      // Deliberately not a snackbar: the engine is not a thing the user
      // chose, so its lifecycle is not news to them.
      debugPrint('engine start failed: $e');
    }
    if (mounted) setState(() {}); // hand the live adapter to the tabs
  }

  /// Sign out: doorbell off, engine down (local DB wiped), Supabase session
  /// gone, back to the sign-in route. Order matters — the push pilot and the
  /// auth listener both hold the adapter, so they let go before it does.
  Future<void> _signOut() async {
    await _authSub?.cancel();
    _authSub = null;
    if (_pushPilotEnabled) await pushPilot.detach();
    await engineRegistry.stop();
    await Supabase.instance.client.auth.signOut();
    if (mounted) Navigator.of(context).pushReplacementNamed('/signin');
  }

  void _notify(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
        .showSnackBar(SnackBar(content: Text(message)));
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

  /// Production wiring for [HistoryScreen.runSuite]: runs the two-engine
  /// comparison (bench/harness.dart's `runFullSuiteForEngines`) against fresh
  /// server-mode and direct-mode Nostos adapters. Deliberately bypasses
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
    await runFullSuiteForEngines(
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
        Engine.nostos: () => NostosAdapter(),
        Engine.nostosDirect: () =>
            NostosAdapter.direct(anonKey: _supabaseAnonKey),
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
            key: const Key('sign-out'),
            icon: const Icon(Icons.logout),
            tooltip: 'Sign out',
            onPressed: _signOut,
          ),
        ],
      ),
      body: TrainingHome(adapter: engineRegistry.current),
    );
  }

  Widget _buildHistoryTab(BuildContext context) {
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
        return HistoryScreen(
          store: store,
          // Lazy: Supabase.instance.client is touched only when Upload is
          // actually tapped, not merely when this tab is built — keeps the
          // tab reachable in widget tests with no Supabase.initialize().
          uploadRuns: (rows) =>
              supabasePostgrestUpload(Supabase.instance.client)(rows),
          runSuite: () => _runBenchSuite(store),
          // The one thing on this tab that does come from the engine: the
          // order history the server writes (migration 0007).
          // No engine yet means no history rather than a spinner that never
          // resolves — same shape ShopScreen uses for a null adapter.
          events:
              engineRegistry.current?.watchOrderEvents() ?? _noOrderEvents(),
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
              _ => _buildHistoryTab(context),
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
            key: Key('nav-tab-history'),
            icon: Icon(Icons.history_outlined),
            selectedIcon: Icon(Icons.history),
            label: 'History',
          ),
        ],
      ),
    );
  }
}
