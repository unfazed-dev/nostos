import 'dart:convert';

import 'package:flutter/foundation.dart'
    show FlutterError, debugPrint, visibleForTesting;
import 'package:flutter/services.dart' show rootBundle;

/// App-level Nostos configuration, normally loaded from a bundled
/// `nostos.json` asset (the analogue of `firebase_options` / Supabase's
/// config block): where the nostos-server lives, optional Supabase-cloud
/// credentials, and the local SQLite filename.
///
/// ```jsonc
/// // assets/nostos.json
/// {
///   "url": "wss://nostos.example.com/sync",   // required — nostos-server /sync
///   "supabase": {                            // optional — Supabase cloud
///     "url": "https://xyz.supabase.co",
///     "anon_key": "eyJ..."
///   },
///   "sqlite_filename": "cairn.sqlite"        // optional — default shown
/// }
/// ```
///
/// Register the asset in `pubspec.yaml` (`assets: [assets/nostos.json]`),
/// then:
///
/// ```dart
/// final config = await NostosConfig.load();           // assets/nostos.json
/// final db = await NostosDatabase.open(
///   config: config,
///   schema: appSchema,       // your declared NostosSchema (the migration story)
///   sqliteDir: dir.path,     // e.g. getApplicationSupportDirectory()
/// );
/// ```
///
/// When the `supabase` block is present, [NostosDatabase.open] initializes
/// Supabase (if the app hasn't already) and forwards the signed-in
/// session's access token as the sync bearer token.
class NostosConfig {
  const NostosConfig({
    required this.url,
    this.supabaseUrl,
    this.supabaseAnonKey,
    this.sqliteFilename = 'cairn.sqlite',
  });

  /// Parse a decoded `nostos.json` map. Throws [FormatException] with a
  /// pointed message when required keys are missing/mistyped, so a bad
  /// config fails loudly at startup rather than as a dangling socket.
  factory NostosConfig.fromJson(Map<String, dynamic> json) {
    final url = json['url'];
    if (url is! String || url.isEmpty) {
      throw const FormatException(
        'nostos config: "url" is required — the nostos-server /sync WebSocket '
        'URL (e.g. "ws://localhost:8800/sync")',
      );
    }
    final scheme = Uri.tryParse(url)?.scheme;
    if (scheme != 'ws' && scheme != 'wss' && scheme != 'iroh') {
      throw FormatException(
        'nostos config: "url" must be a ws://, wss://, or iroh:// URL, '
        'got "$url"',
      );
    }
    String? supabaseUrl;
    String? supabaseAnonKey;
    final supabase = json['supabase'];
    if (supabase != null) {
      if (supabase is! Map<String, dynamic>) {
        throw const FormatException(
          'nostos config: "supabase" must be an object with "url" and '
          '"anon_key"',
        );
      }
      supabaseUrl = supabase['url'] as String?;
      // `publishable_key` is Supabase's successor name for the anon key;
      // accept either spelling.
      supabaseAnonKey =
          (supabase['anon_key'] ?? supabase['publishable_key']) as String?;
      if (supabaseUrl == null || supabaseAnonKey == null) {
        throw const FormatException(
          'nostos config: "supabase" requires both "url" and "anon_key" '
          '(or "publishable_key")',
        );
      }
    }
    final filename = json['sqlite_filename'] as String? ?? 'cairn.sqlite';
    return NostosConfig(
      url: url,
      supabaseUrl: supabaseUrl,
      supabaseAnonKey: supabaseAnonKey,
      sqliteFilename: filename,
    );
  }

  static const _defaultAsset = 'assets/nostos.json';
  static const _legacyAsset = 'assets/cairn.json'; // rename:hold — pre-rename asset name, read as fallback until 1.0 (ADR-0046)

  /// Load and parse a bundled JSON asset (default `assets/nostos.json`, or
  /// the pre-rename asset when only that one is bundled — ADR-0046).
  ///
  /// The asset must be registered under `flutter/assets` in the app's
  /// `pubspec.yaml`. Throws [FlutterError] if the asset is missing and
  /// [FormatException] if it fails validation (see [NostosConfig.fromJson]).
  static Future<NostosConfig> load({String asset = _defaultAsset}) async {
    final legacy = asset == _defaultAsset ? _legacyAsset : asset;
    final raw = await loadAssetOrLegacy(rootBundle.loadString, asset, legacy);
    return NostosConfig.fromJson(jsonDecode(raw) as Map<String, dynamic>);
  }

  /// nostos-server `/sync` URL — `ws://`/`wss://` (the default transport) or
  /// an `iroh://…?ticket=…` dial URL (ADR-0041 preview: requires the native
  /// library built with the `iroh` feature, which shipped artifacts keep OFF
  /// until ADR-0041's field-leg condition clears; the Rust side fails loudly
  /// if the feature is missing).
  final String url;

  /// Supabase project URL — set together with [supabaseAnonKey] to run
  /// against Supabase cloud (auth token forwarded to the sync connection).
  final String? supabaseUrl;

  /// Supabase anon (publishable) key — see [supabaseUrl].
  final String? supabaseAnonKey;

  /// Local SQLite filename, joined onto the directory the app passes to
  /// [NostosDatabase.open] (`sqliteDir`). Default `cairn.sqlite`.
  final String sqliteFilename;

  /// Whether this config carries Supabase-cloud credentials.
  bool get hasSupabase => supabaseUrl != null && supabaseAnonKey != null;
}

/// `asset` via [load], or `legacy` when `asset` is missing (ADR-0046). With
/// equal names the missing-asset error propagates unchanged.
@visibleForTesting
Future<String> loadAssetOrLegacy(
  Future<String> Function(String) load,
  String asset,
  String legacy,
) async {
  try {
    return await load(asset);
  } on FlutterError {
    if (legacy == asset) rethrow;
    debugPrint(
      'warning: $legacy is deprecated, rename it to $asset '
      '(read as a fallback until 1.0)',
    );
    return load(legacy);
  }
}
