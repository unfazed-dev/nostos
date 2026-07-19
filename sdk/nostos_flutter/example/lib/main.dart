// Nostos Provider Dashboard — offline-first multi-table booking app (Flutter).
//
// A production-quality booking application showcasing the nostos Flutter SDK:
//   - 6 tables on ONE /sync socket: providers, clients, availabilities,
//     appointments, invoices, messages (D1/ADR-0022 multi-table subscribe).
//   - Reactive typed watch → watchMapped<T>('SELECT * FROM <table>', fromRow)
//     per NavigationRail/BottomNav tab. IndexedStack preserves state across
//     tab switches (the reactive-stream fix).
//   - Durable offline writes → write(table, op, pk, payload) lands in the local
//     SQLite outbox + flushes on reconnect (ADR-0013).
//   - REAL pause/resume (D2) → disconnect() aborts ONLY the /sync loop; reads,
//     writes, and the UI keep working offline.
//   - Auto-calculated billing → invoices compute from provider rates (hourly /
//     flat / subscription) via BillingService; the rate is snapshotted at issue.
//   - Realtime chat → the synced `messages` table IS the realtime stream (no
//     separate WebSocket; 2026 local-first best practice).
//   - Connection state → aggregate badge across the shared session.
//
// Architecture: stacked MVVM methodology + Material 3 (the stacked-kit-designer
// skill's anti-slop blacklist + responsive form-factor thinking applied, but
// with Material 3 + cupertino idioms instead of the unavailable stacked_kit
// tokens — stacked_kit does not exist on pub.dev).
//
// Backend: `nostos dev` binds ws://127.0.0.1:8800; override via --dart-define=
// NOSTOS_URL=... Writes round-trip only when NOSTOS_WRITE_TABLES lists the 6
// tables (see example/README.md).

import 'dart:async';
import 'dart:io';

import 'package:nostos_flutter/nostos_flutter.dart';
import 'package:flutter/material.dart';

import 'app/app_theme.dart';
import 'nostos.g.dart' show nostosConfig, nostosSchema;
import 'views/dashboard_shell.dart';

/// Optional /sync URL override:
/// `flutter run --dart-define=NOSTOS_URL=ws://host:port/sync`.
/// When empty, the URL comes from `nostosConfig` (generated from `.nostos/config.json`
/// by `nostos gen`).
const _kUrlOverride = String.fromEnvironment('NOSTOS_URL');

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(const NostosDashboardApp());
}

class NostosDashboardApp extends StatelessWidget {
  const NostosDashboardApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Nostos Provider Dashboard',
        debugShowCheckedModeBanner: false,
        theme: AppTheme.light,
        darkTheme: AppTheme.dark,
        home: const DashboardBoot(),
      );
}

/// Boots the NostosDatabase connection before showing the DashboardShell.
/// Shows a loading spinner (or the connect-failed error) while booting.
class DashboardBoot extends StatefulWidget {
  const DashboardBoot({super.key});
  @override
  State<DashboardBoot> createState() => _DashboardBootState();
}

class _DashboardBootState extends State<DashboardBoot> {
  NostosDatabase? _db;
  String? _error;

  @override
  void initState() {
    super.initState();
    _boot();
  }

  Future<void> _boot() async {
    try {
      var config = nostosConfig;
      if (_kUrlOverride.isNotEmpty) {
        config = NostosConfig(
          url: _kUrlOverride,
          supabaseUrl: config.supabaseUrl,
          supabaseAnonKey: config.supabaseAnonKey,
          sqliteFilename: config.sqliteFilename,
        );
      }
      final db = await NostosDatabase.open(
        config: config,
        schema: nostosSchema,
        sqliteDir: Directory.systemTemp.path,
      );
      await db.subscribeTables(kTables);
      if (!mounted) return;
      setState(() => _db = db);
    } catch (e) {
      if (!mounted) return;
      setState(() => _error = e.toString());
    }
  }

  @override
  Widget build(BuildContext context) {
    if (_db != null) {
      return DashboardShell(db: _db!);
    }
    return Scaffold(
      body: Center(
        child: _error == null
            ? const Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  CircularProgressIndicator(),
                  SizedBox(height: 16),
                  Text('Connecting to nostos…',
                      style: TextStyle(fontSize: 13)),
                ],
              )
            : Padding(
                padding: const EdgeInsets.all(24),
                child: Text('Connect failed:\n$_error',
                    style: const TextStyle(color: Colors.red)),
              ),
      ),
    );
  }
}
