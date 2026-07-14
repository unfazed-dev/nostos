import 'engine.dart' show ClientTableFfi;

/// The server's table schema for a publication, resolved at connect time.
///
/// This is the Dart-side mirror of the nostos-server `GET /schema` response
/// (ADR-0019): a list of [Table]s, each with a primary key and ordered
/// column names. It exists so [NostosDatabase.connect] can call
/// `Nostos.applySchema` with a typed value rather than a hand-rolled list of
/// `ClientTableFfi`s, and so the app can inspect the resolved shape without
/// re-fetching.
///
/// Build it either by parsing the server descriptor ([Schema.fromSchemaDescriptor])
/// — which [NostosDatabase.connect] does automatically when no `schema` is
/// passed — or by constructing `Table`s directly for tests / fixed-shape
/// apps. Convert to the FFI mirror with [toClientTables].
class Schema {
  const Schema({required this.tables});

  /// Tables in publication order.
  final List<Table> tables;

  /// Parse the nostos-server `GET /schema` response body.
  ///
  /// Expected shape:
  /// ```
  /// {
  ///   "publication": String,             // carried on the wire; unused here
  ///   "tables": [
  ///     {
  ///       "name": String,
  ///       "primary_key": [String, ...],
  ///       "columns": [
  ///         { "name": String, "pg_oid": int, "affinity": String }
  ///       ]
  ///     }, ...
  ///   ]
  /// }
  /// ```
  /// `pg_oid` is informational only (it's the source-of-truth type tag from
  /// Postgres) and is dropped here — the WS2 view path keys reads off column
  /// *names* via `json_extract`, not off OIDs.
  factory Schema.fromSchemaDescriptor(Map<String, dynamic> json) {
    final rawTables = (json['tables'] as List<dynamic>).cast<Map<String, dynamic>>();
    return Schema(
      tables: rawTables.map(Table._fromJson).toList(growable: false),
    );
  }

  /// Map this schema to the FFI mirror consumed by `Nostos.applySchema` (which
  /// creates one `CREATE VIEW` per table). The WS2 views are name-keyed
  /// `json_extract` projections, so column names are all the view layer needs.
  List<ClientTableFfi> toClientTables() => tables
      .map(
        (t) => ClientTableFfi(
          name: t.name,
          primaryKey: t.primaryKey,
          columns: t.columns,
        ),
      )
      .toList(growable: false);
}

/// One table in a resolved [Schema].
class Table {
  const Table({
    required this.name,
    required this.primaryKey,
    required this.columns,
  });

  /// Canonical table id (matches `cairn_data.table_name` / the wire `table`).
  final String name;

  /// Primary-key column names. Informational for the current WS2 view path
  /// (reads are name-keyed); carried for the future materialized-table path.
  final List<String> primaryKey;

  /// Column names in tuple order (the JSON keys inside `cairn_data.payload`).
  /// WS6 typed-record codegen will promote this to a typed list carrying
  /// affinity/pg_oid; today the WS2 view path keys reads off names only, so a
  /// bare name list is the minimal carrier (no `Column` wrapper — YAGNI).
  final List<String> columns;

  factory Table._fromJson(Map<String, dynamic> json) => Table(
        name: json['name'] as String,
        primaryKey: (json['primary_key'] as List<dynamic>).cast<String>(),
        columns: (json['columns'] as List<dynamic>)
            .cast<Map<String, dynamic>>()
            .map((c) => c['name'] as String)
            .toList(growable: false),
      );
}
