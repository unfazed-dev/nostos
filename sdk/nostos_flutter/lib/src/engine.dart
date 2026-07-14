/// The seam between the public [NostosEngine]-typed API in `nostos.dart` and
/// the generated flutter_rust_bridge bindings.
///
/// Why this exists: the generated `NostosHandle.subscribe` takes a
/// `RustStreamSink<T>`, whose `.stream` getter only works after the FFI call
/// has run `setupAndSerialize` on it (see
/// `flutter_rust_bridge/src/stream/stream_sink.dart`) — a pure-Dart test
/// double cannot satisfy that type without loading the native library. This
/// interface deals only in plain Dart `Stream`s, so a unit test can inject a
/// [NostosEngine] fake and exercise `Nostos`'s API surface (subscribe/watch/
/// write wiring, error paths, table-mismatch checks) with zero native
/// dependency. [RustNostosEngine] is the one adapter that actually talks to
/// Rust; it's the only file in this package that imports
/// `src/rust/api/nostos.dart`.
library;

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart';

import 'rust/api/nostos.dart' as rust;

// Re-exported so the public API in `nostos.dart` (`Nostos.applySchema`) and
// `schema.dart` (`Schema.toClientTables`) can name [ClientTableFfi] without
// each importing `rust/api/nostos.dart` directly — keeping this file the sole
// importer of the generated bindings (see the library doc above).
export 'rust/api/nostos.dart' show ClientTableFfi;

/// Connection-state transitions, decoupled from the generated
/// `rust.NostosConnectionState` so consumers never need to import generated
/// code. See `rust/src/api/nostos.rs`'s `NostosConnectionState` doc for the
/// precise (heuristic) semantics of `connected`.
enum NostosConnectionState { connecting, connected, reconnecting, disconnected }

/// The two streams a subscription produces.
class NostosSubscriptionStreams {
  const NostosSubscriptionStreams({required this.rows, required this.state});

  /// One JSON-array-of-objects string per tick — the full row set for the
  /// subscribed table.
  final Stream<String> rows;

  /// Connection-state transitions for this subscription's session.
  final Stream<NostosConnectionState> state;
}

/// What [Nostos] needs from a backend: start a subscription, perform a
/// durable write. Implemented for real by [RustNostosEngine]; implement it
/// yourself in tests to avoid the native library entirely.
abstract class NostosEngine {
  Future<NostosSubscriptionStreams> subscribe({
    required String table,
    String? whereSql,
  });

  /// Returns the local outbox id.
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  });

  /// Run an arbitrary SELECT against on-device SQLite. Returns a JSON-array
  /// string (same shape as [NostosSubscriptionStreams.rows]); decode with
  /// jsonDecode. Requires an active subscription.
  Future<String> query({required String sql});

  /// Materialize the WS2 read-views for [tables] in the on-device SQLite
  /// file (`CREATE VIEW IF NOT EXISTS <table> AS SELECT json_extract(...)
  /// ... FROM cairn_data WHERE table_name='<table>'` — see
  /// `SqliteStorage::apply_schema`). Idempotent for an unchanged schema;
  /// the views persist in the SQLite file, so this only needs to run once
  /// after connect. Synchronous: the FFI is `Result<(), String>` and throws
  /// on error. Wraps the generated `NostosHandle.applySchema`.
  void applySchema(List<rust.ClientTableFfi> tables);

  /// Tear down the active subscription's background work (the sync loop and
  /// the watch-stream pump). Safe to call with no active subscription and
  /// safe to call more than once.
  Future<void> close();
}

/// The real engine: wraps the generated `rust.NostosHandle`.
class RustNostosEngine implements NostosEngine {
  RustNostosEngine._(this._handle);

  /// Opens a connection (no network activity yet — see `NostosHandle.connect`
  /// in the Rust glue).
  factory RustNostosEngine.connect({
    required String url,
    String? token,
    required String dbPath,
  }) => RustNostosEngine._(
    rust.NostosHandle.connect(url: url, token: token, dbPath: dbPath),
  );

  final rust.NostosHandle _handle;

  @override
  Future<NostosSubscriptionStreams> subscribe({
    required String table,
    String? whereSql,
  }) async {
    final rowsSink = RustStreamSink<String>();
    final stateSink = RustStreamSink<rust.NostosConnectionState>();
    await _handle.subscribe(
      table: table,
      whereSql: whereSql,
      rowsSink: rowsSink,
      stateSink: stateSink,
    );
    return NostosSubscriptionStreams(
      rows: rowsSink.stream,
      state: stateSink.stream.map(_mapState),
    );
  }

  @override
  Future<int> write({
    required String table,
    required String op,
    required String pk,
    String? payloadJson,
  }) async {
    final id = await _handle.write(
      table: table,
      op: op,
      pk: pk,
      payloadJson: payloadJson,
    );
    return id.toInt();
  }

  @override
  Future<String> query({required String sql}) =>
      _handle.query(sql: sql);

  @override
  void applySchema(List<rust.ClientTableFfi> tables) =>
      _handle.applySchema(tables: tables);

  @override
  Future<void> close() => _handle.close();
}

NostosConnectionState _mapState(rust.NostosConnectionState s) => switch (s) {
  rust.NostosConnectionState.connecting => NostosConnectionState.connecting,
  rust.NostosConnectionState.connected => NostosConnectionState.connected,
  rust.NostosConnectionState.reconnecting => NostosConnectionState.reconnecting,
  rust.NostosConnectionState.disconnected => NostosConnectionState.disconnected,
};
