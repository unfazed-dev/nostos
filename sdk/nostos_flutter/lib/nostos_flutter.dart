/// nostos_flutter — plug-and-play local-first sync for Flutter, backed by a
/// Rust `nostos-client` (SQLite + WebSocket sync loop) via
/// flutter_rust_bridge's native-assets backend.
///
/// Start at [NostosDatabase] — `NostosDatabase.open` / `.supabase` is the taught
/// entry point. It resolves the server schema for you (`GET /schema`), so
/// `SELECT * FROM <table>` works immediately, and adds typed
/// [Collection]-per-table handles and a [SyncStatus] signal.
///
/// No connector class and no *hand-written* schema: `subscribe` sets the
/// server-side predicate, `watch` gives you a reactive `Stream` of rows, and
/// `write` applies locally at once and syncs in the background through a
/// durable outbox. A [NostosSchema] is optional — pass one only to constrain or
/// pin what the server reports. See the package README for the quickstart.
///
/// [Nostos] is the low-level engine handle underneath [NostosDatabase]. It stays
/// exported as an escape hatch (and is the seam tests fake against), but it is
/// deliberately not the documented path — prefer [NostosDatabase] unless you have
/// a reason not to.
library;

// `Nostos` is the low-level handle; `NostosDatabase` (below) is the taught surface.
export 'src/nostos.dart' show Nostos, NostosSupabase, NostosConnectionState, NostosTableSub;
export 'src/nostos_config.dart' show NostosConfig;
// `Table` and `Column` are intentionally NOT re-exported at the package
// root because they shadow Flutter's `Table`/`Column` widgets (a hard
// collision for any app importing both this package and `material.dart`).
// Declare app schemas with the collision-free aliases `NostosTable` /
// `NostosColumn` instead (same classes).
export 'src/schema.dart' show NostosSchema, NostosTable, NostosColumn;
export 'src/predicate.dart' show Where, Order;
export 'src/nostos_database.dart'
    show
        NostosDatabase,
        Collection,
        SyncStatus,
        NostosWrite,
        DeadLetter,
        WriteBatchPartialError;
