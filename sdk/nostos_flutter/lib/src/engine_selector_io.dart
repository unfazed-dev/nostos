/// Native [NostosEngine] factory — the default (non-web) conditional-import
/// arm (ADR-0036). See `engine_selector.dart` for the selection rationale.
library;

import 'dart:async';

import 'package:path_provider/path_provider.dart';

import 'engine.dart';
import 'engine_direct.dart';
import 'engine_io.dart';
import 'rust/frb_generated.dart';

/// `RustLib.init()` is idempotent but not free; gate it behind a one-shot flag
/// (mirrors the prior in-class static on `Nostos`, relocated here so `nostos.dart`
/// no longer imports the frb barrel directly).
bool _rustInitialized = false;

/// Create the native [NostosEngine] (flutter_rust_bridge). Initializes the Rust
/// runtime once (idempotent) and resolves the on-device SQLite path via
/// [path_provider] when [sqlitePath] is omitted. [workerUrl] is web-only and
/// ignored here (kept in the signature so both selectors match).
Future<NostosEngine> createNostosEngine({
  required String url,
  String? token,
  String? sqlitePath,
  String? workerUrl,
}) async {
  if (!_rustInitialized) {
    await RustLib.init();
    _rustInitialized = true;
  }
  final path = sqlitePath ?? await _defaultSqlitePath(url);
  return RustNostosEngine.connect(url: url, token: token, dbPath: path);
}

Future<String> _defaultSqlitePath(String url) async {
  final dir = await getApplicationSupportDirectory();
  final safeName = url.replaceAll(RegExp(r'[^A-Za-z0-9]+'), '_');
  return '${dir.path}/nostos_$safeName.sqlite';
}

/// Create the direct-mode [NostosEngine] ([DirectNostosEngine]): device →
/// Supabase, no `nostos-server` process (ADR-0045). Initializes the Rust runtime
/// once, exactly like [createNostosEngine].
///
/// The SQLite path is keyed off the project URL, not the [scope]: signing out
/// wipes the file (`signOut`), so one device-per-project file is enough and a
/// per-user file would only leak the previous user's rows onto disk.
Future<NostosEngine> createDirectNostosEngine({
  required String supabaseUrl,
  required String anonKey,
  required String scope,
  String? token,
  String? sqlitePath,
  Map<String, String> counterFields = const <String, String>{},
  bool keepLocalOnSignOut = false,
}) async {
  if (!_rustInitialized) {
    await RustLib.init();
    _rustInitialized = true;
  }
  final path = sqlitePath ?? await _defaultSqlitePath(supabaseUrl);
  return DirectNostosEngine.connect(
    supabaseUrl: supabaseUrl,
    anonKey: anonKey,
    scope: scope,
    token: token,
    dbPath: path,
    counterFields: counterFields,
    keepLocalOnSignOut: keepLocalOnSignOut,
  );
}
