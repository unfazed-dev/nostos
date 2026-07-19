/// nostos_flutter — plug-and-play local-first sync for Flutter, backed by a
/// Rust `nostos-client` (SQLite + WebSocket sync loop) via
/// flutter_rust_bridge's native-assets backend.
///
/// No connector class, no client-side schema artifact: `subscribe` sets the
/// server-side predicate, `watch` gives you a reactive `Stream` of rows,
/// `write` is a durable local outbox. See the package README for the
/// quickstart.
library;

export 'src/nostos.dart' show Nostos, NostosSupabase, NostosConnectionState, NostosTableSub;
export 'src/nostos_config.dart' show NostosConfig;
// `Table` and `Column` are intentionally NOT re-exported at the package
// root because they shadow Flutter's `Table`/`Column` widgets (a hard
// collision for any app importing both this package and `material.dart`).
// Declare app schemas with the collision-free aliases `NostosTable` /
// `NostosColumn` instead (same classes).
export 'src/schema.dart' show NostosSchema, NostosTable, NostosColumn;
export 'src/nostos_database.dart' show NostosDatabase, Collection, SyncStatus;
