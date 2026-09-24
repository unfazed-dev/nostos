# ADR-0021: Client schema discovery via REST (`GET /schema`)

- **Status:** Accepted (shipped)
- **Date:** 2026-07-13

## Context

WS1 of the Flutter reactive-query redesign (Option-C) needs the client to
discover the publication's typed schema (tables, columns, SQLite affinities) to
auto-build its typed read surface — the headline DX win: no hand-written
`Schema` to maintain. (This originally read "auto-build its typed **tables**",
which the client never did and now never will: the descriptor drives one SQLite
**VIEW** per table over the opaque `nostos_data` payload, and materialized typed
tables are rejected — [ADR-0028](0028-client-read-views-over-opaque-payload.md).
Nothing about *this* decision changes; only the consumer's shape.) nostos-server already bootstraps this metadata from the Postgres
catalog (`PgReplicator::catalog_relations` → `RelationMeta`, ADR-0019), but it
was private to the replicator — unreachable from the client or even from
`nostos-server`'s own HTTP surface. ADR-0019 explicitly named this as the
deferred upgrade ("a relation-metadata wire frame so the client stops
guessing").

Two ways to expose it:

1. **REST** — a side-band `GET /schema` route, alongside `/healthz` and
   `/metrics`.
2. **Wire frame** — emit a schema frame on the WebSocket (on connect or
   subscribe), adding a third wire shape after `WireFrame` and `write_result`.

## Decision

Ship **`GET /schema`** (REST). The schema is *publication-wide* (the set of
synced tables), not per-subscribe-table, so emitting it on the only existing
wire moment (subscribe) is wasteful and the subscribe frame carries a single
table. There is no wire init/hello frame today — ADR-0014's snapshot-on-subscribe
injected synthetic `Insert` events into the existing `WireFrame` stream rather
than add a frame type — so a schema wire frame would be a brand-new third shape
with real pre-v1 wire-compat cost. The side-band GET pattern is already
established (`/healthz`, `/metrics`, both CORS-handled), and REST-schema + WS
changes is exactly the split used by comparable sync engines (the parity
target).

Surface, mirroring `SnapshotSource` (ADR-0014):

- Port `SchemaSource` + DTOs (`SchemaDescriptor` / `SchemaTable` / `SchemaColumn`)
  + `SchemaError` in `nostos-application/ports.rs`. The DTOs live with the port,
  not in `nostos-domain` — they are transport-shaped (serialized to the client),
  not domain invariants with behavior.
- Adapter `PgSchemaSource` in `nostos-infra/schema_source.rs` (calls the existing
  `pub(crate) catalog_relations` same-crate; no visibility change). Reports the
  **real** primary key per table from `pg_index.indisprimary` — not the hardcoded
  `"id"` that `PgSnapshotter` / `PgWriteBack` guess at today.
- Helper `typed::oid_to_sqlite_affinity` (ADR-0019's named deferred upgrade) —
  affinity mirrors the JSON token shape `append_typed_value` emits, so the wire
  value stores in the client's typed column without coercion. Notably
  `int8` / `numeric` → `TEXT` (ADR-0019 renders them as strings for precision).
- `GET /schema` handler in `nostos-server/main.rs`, wired under
  `#[cfg(feature = "pg")]`. Returns 404 when no `SchemaSource` is wired
  (fake / no-`pg`), 503 on backend error, 200 JSON otherwise.

## Consequences

- **v1 unauthenticated.** Schema is publication-wide metadata, not tenant-scoped
  rows; row isolation is the read-path predicate's job (ADR-0011 / ADR-0018). A
  managed deploy that wants to hide publication metadata adds auth at the route
  layer (cheap, deferred — `// v2: add auth` marker at route registration).
- **Forward-compatible JSON.** Each column carries `pg_oid` so a future client
  can resolve enum labels / array element types (ADR-0019 deferred work) without
  a schema-version bump.
- **The real PK now flows to the client**, unblocking a later fix to
  `PgSnapshotter` / `PgWriteBack`'s hardcoded `"id"` PK (WS2 consumes it; out of
  scope for WS1).

## References

- Supersedes the deferral noted in ADR-0019 ("relation-metadata wire frame ...
  so the client stops guessing at parse time").
- Mirrors: `SnapshotSource` / `PgSnapshotter` (ADR-0014).
