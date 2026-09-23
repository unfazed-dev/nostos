# P5 — Sync Streams design (DRAFT)

- **Status:** IMPLEMENTED 2026-08-18 (ADR-0039). Slices landed on `main`: 1 domain
  (`2b282b3`), 2 rules (`0a500b8`), 3 wire (`9328047`), 4 snapshot port+adapter
  (`fd30612`), 5 transport (`d717beb`), 6 server (`628feb7`), 7 client (`5d3f898`),
  8 Flutter (`7dbb687`), 9 e2e (`5b43ff7` — compiled, self-skips; live-PG run
  blocked by Docker Desktop VM corruption on the dev machine). Closes the biggest remaining feature gap
  in the now-superseded SDK-parity plan.
- **Goal:** named, per-client-parameterized sync streams with lazy
  `syncStream(name, params).subscribe()` — without weakening ADR-0011/0018 tenancy.

## Decisions (summary)

1. **Streams are server-defined, client-parameterized.** A stream is a named config entry
   `{ name, table, predicate_template }`; the template is the ADR-0012 safe-SQL-subset
   grammar extended with `:param` placeholders in literal position. Clients never send
   SQL — they send a name + a JSON params object.
2. **Parameter binding is value-level, never textual (the injection answer).** The
   template is parsed ONCE at server startup by an extended `parse_predicate_expr`
   (`crates/nostos-domain/src/predicate_compile.rs:64`) into a `PredicateExpr`
   (`crates/nostos-domain/src/predicate.rs:100`) with a new `ColumnValue::Param(name)`
   marker leaf. At subscribe time each param value becomes a typed `ColumnValue` LEAF —
   the shape ADR-0012 slice-2 already type-checks at match time. No client byte ever
   enters SQL on the live path (predicates evaluate in memory, `fanout.rs:265`). On the
   SNAPSHOT path (real SQL), values
   go through tokio_postgres positional `$n` binds — never `format!` — a fourth defense
   added to the three at `snapshot_source.rs:15-30`. Bad params → rejected like
   `InvalidWhereSql` (`transport.rs:1189`).
3. **Tenancy is unchanged.** A stream's bound predicate folds into `build_predicate`
   (`transport.rs:1225`) at exactly the `where_sql` seam (:1273-1286) — BEFORE the
   tenant clause wraps everything LAST (:1288-1294). A param naming the tenant column is
   dropped + overridden like a `filters` entry (:1262-1266). Params can only narrow.
4. **v1 scope.** IN: single-table parameterized predicates (six operators + AND/OR/NOT +
   placeholders), multiple named streams per socket, lazy add/drop, per-stream targeted
   snapshot. OUT: JOIN/CTE membership + re-evaluation. **Why OUT is honest:** replication
   events arrive per-table, one row image at a time (`replicator/pg.rs` → `ReplicationEvent`);
   a JOIN predicate can't be evaluated against one row's payload, and a row entering/
   leaving a JOIN result without itself changing emits no event on the streamed table.
   **ponytail ceiling:** single-table templates are exact; JOIN/CTE-shaped definitions are
   rejected at startup with a clear error. **Upgrade path:** (a) dependency-table
   re-evaluation (reverse index table→streams, bounded EXISTS re-query — measure first);
   (b) PG-side materialized view per stream.
5. **Wire: additive frames on the existing tagged-JSON protocol** (the
   `write_result`/`resume_info` pattern, `crates/nostos-infra/src/wire.rs:158-162,371-391`).
6. **Snapshot is targeted per stream, never a re-snapshot** (§3).
7. **Client adds a mid-session subscribe channel** (today subscribes are sent only at
   session start, `crates/nostos-client/src/client.rs:1073-1104`).

## 1. Wire protocol additions (`crates/nostos-infra/src/wire.rs`)

New `ClientMessage` variants (:44), serde-tagged, all additive:

```json
{"type":"subscribe_stream","id":"s1","stream":"lists","params":{"owner":"u1"}}
{"type":"unsubscribe_stream","id":"s1"}
```

- `id` is client-chosen (unique per socket; reuse = idempotent replace).
- No per-stream `resume_lsn`/`epoch`: streams ride the socket's ONE global checkpoint
  (ADR-0009; `client.rs:70-75`); a lazy add takes a targeted snapshot (§3).
- Row frames UNCHANGED: a streamed row is an ordinary `WireFrame` (:20-30), applied by
  pk idempotently (ADR-0013 echo). Two streams on one table share the socket sink, whose
  LSN dedup ring already tolerates overlap (`transport.rs:939-942`).
- Snapshot boundary frames gain one optional field:
  `{"type":"snapshot_begin","table":"lists","stream":"s1"}`. `decode_control_frame`
  (:352) ignores unknown keys (:393 — forward-compat), so old clients are safe.
- Server→client `{"type":"stream_error","id":"s1","error":"..."}` for mid-session
  rejects (unknown stream, bad params) — mirrors the non-fatal reject at `transport.rs:723`
  instead of a socket close.
- Back-compat: old `subscribe` frames (:46-78) untouched. An OLD server closes on the
  unknown `type` (`decode_client_message` → `None`, :121-123) — a documented hard SDK error.

## 2. Server: stream definition → per-client filter

- **Definition source:** a `[streams]` section in `nostos_rules.toml` (ADR-0031), hot-
  reloaded with the rules watcher (`transport.rs:254`). Entry:
  `{ table, where = "owner_id = :owner AND priority >= :min" }`. Startup validation:
  `parse_predicate_expr` must accept it, every placeholder in literal position; JOIN/CTE
  keywords fail the grammar — config errors are loud at boot, never at subscribe.
- **Per-subscribe compile:** `subscribe_stream` → look up definition → bind params into
  `ColumnValue` leaves → `Predicate{table, expr}` → `build_predicate` tenant/rules
  wrap (Decision 3) → ONE `SyncSession` per stream instance via `SessionManager::connect`
  (`crates/nostos-application/src/session.rs:46`), exactly as `register_subscribe` does
  (`transport.rs:828-861`). The store indexes by `predicate.table`
  (`crates/nostos-infra/src/store.rs:39-46`), so fan-out stays O(rows × matching sessions)
  (`fanout.rs:19`); two streams on one table = two entries in that table's DashMap shard
  (ponytail: linear-in-streams, fine under the 32 cap; revisit only with a measurement —
  the ADR-0012 digest-index revert precedent).
- **Caps:** streams count against `MAX_TABLES_PER_SOCKET = 32` (`transport.rs:71`) —
  same DoS rationale (each lazy add = one snapshot SELECT).
- **Mid-session routing:** the reader already routes mid-socket `Subscribe`
  (`transport.rs:653-743`); the two new frames join that match. Unsubscribe removes the
  session from the store and (v1) leaves local rows in place — eviction is separate;
  comparable engines behave the same.

## 3. Snapshot semantics for a lazily-added stream
**Targeted per-stream snapshot, never a full re-snapshot.** Extend `SnapshotSource`
(`crates/nostos-application/src/ports.rs:640-656`) with `snapshot_stream(table, where_clause, binds, base_lsn)`:

- The server template compiles to `WHERE <shape with $1..$n>` (SHAPE is trusted config;
  only `:param` VALUES become binds) PLUS — always, when a tenant column is configured —
  `AND "<tenant_col>" = $k` bound from the principal. This closes, for stream snapshots,
  the known unfiltered-snapshot ponytail at `ports.rs:633-638` (today's `PgSnapshotter`
  runs unscoped `SELECT *`, `snapshot_source.rs:197`; table-wide snapshots unchanged).
- Delivery reuses the existing bracket + backpressure-aware path
  (`transport.rs:944-984`: begin/end on the same FIFO channel, `deliver_awaiting`),
  with `stream` set (§1). The socket's `synthetic_cursor` (:943, :986-989) stamps
  snapshot LSNs so cross-stream ranges never collide on the shared sink's dedup ring.
- Live fan-out starts at registration BEFORE the snapshot query (:858-861); the client's
  per-row LSN gate + idempotent upsert make the overlap safe — the same argument as op-log
  replay (:866-868). A failed stream snapshot is non-fatal (:974-979).

## 4. Client API (`crates/nostos-client`, `sdk/nostos_flutter`)

**Rust (`client.rs`):** `client.sync_stream("lists", json!({"owner":"u1"})).subscribe().await?`
returns a `StreamHandle` with `.unsubscribe()` (drop also unsubscribes).

- New mid-session command mpsc into the session task carrying ad-hoc subscribe/
  unsubscribe frames — the write path already proves a client→session-task queue (outbox
  flush, `client.rs:1114-1116`). Active streams are client state; `run_with_reconnect`
  (:1441) re-sends them after the primary subscribe on every reconnect (each re-add takes
  a fresh targeted snapshot — no per-stream resume in v1; ponytail: the socket checkpoint
  + idempotent apply already prevents duplicate rows).
- `SyncClientConfig`-declared streams are sugar for subscribing at connect.

**Flutter (frb, ADR-0015):** mirror the existing pattern — `NostosHandle::subscribe`
takes `Vec<TableSubFfi>` + a `StreamSink`
(`sdk/nostos_flutter/rust/src/api/nostos.rs:302-308`), Dart wraps it
(`sdk/nostos_flutter/lib/src/nostos.dart:120-140`). Add frb methods
`subscribe_stream(name, params_json) -> String` (stream id) + `unsubscribe_stream(id)`
(params as a JSON STRING — the same no-codegen trick P3 used for `op`, parity plan
:106). Dart surface, sync-stream-shaped:

```dart
final sub = db.syncStream('lists', {'owner': uid}).subscribe();
await sub.unsubscribe();
```

Rows surface through the EXISTING reactive layer (`watchQuery`/`watch`,
`nostos.dart:149,221`) — streams control which rows land in SQLite; no new Dart
reactivity. Note: today's bridge REPLACES the session on re-subscribe
(`api/nostos.rs:312-313`); stream add/drop must NOT — it sends frames on the live session.

## 5. Interactions

- **Predicate subscriptions:** fully compatible — same store, sink, session type;
  `where_sql` and streams coexist on one socket; no frame changes for old clients.
- **Write-back (ADR-0013/0018):** untouched. Streams are read-path only; writes still
  route via the `NOSTOS_WRITE_TABLES` allowlist + tenant force-stamp
  (`crates/nostos-server/src/main.rs:755-774`); stream rows echo back by pk.
- **Sharded-router ceiling:** no change to the table-sharded index (`store.rs:1-10`);
  stream sessions are ordinary sessions in the table shard. The 833k ops/sec claim is
  per-session predicate matching, which streams reuse — but a P5 bench row (streams ×
  params) is required before marketing reuses the number.
- **Rules modes (ADR-0031):** a stream on a `NotSynced` table is rejected through the
  same fail-closed `build_predicate` path (`transport.rs:1233-1245`).

## 6. Test plan

Unit (no PG): codec round-trips for all 3 new frames + boundary `stream` field; startup
validation rejects JOIN/CTE/subquery/misplaced-placeholder shapes; binding rejects
missing/extra/mistyped params; a param on the tenant column is overridden (extend the
harness at `transport.rs:1886-2006`); defensive absent-column semantics unchanged.

PG-gated e2e — `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=... cargo test -p nostos-infra --features pg
-- --test-threads=1` (mandatory serialization; the shared `tasks` table is TRUNCATEd):
1. Lazy mid-session stream: subscribe base table, add stream → targeted snapshot + live
  delta arrive; rows outside the bound params do not.
2. Unsubscribe stops flow: matching post-unsubscribe mutations never arrive; other
  streams on the socket keep flowing.
3. **Cross-tenant parameter abuse (the hard gate):** authenticated as tenant A, send
  `params` attempting escape — `{"org_id":"tenant-b"}` on a tenant-column placeholder,
  and metacharacter values (`"x' OR '1'='1"`, `"; DROP TABLE tasks;--"`). Assert: only
  tenant-A rows ever arrive; the snapshot returns data-or-empty, never an interpolation
  error. Mirrors `e2e_pg_writeback.rs:576`'s cross-tenant style.
4. Two streams, same table, different params, one socket: each row applies once (dedup
  ring); both param sets honored.
5. Reconnect: active streams re-subscribe + re-snapshot; no duplicate rows (LSN gate).
6. Stream on a rules-denied (`toggles`) table → `stream_error`, socket stays up.

## 7. Implementation checklist

- [x] `predicate_compile.rs` (nostos-domain): `:param` placeholder tokens in literal
  position; `ColumnValue::Param(String)` marker; bind step → typed leaves. §6 unit tests.
- [x] `rules_file.rs` (nostos-infra) + `rules.rs` (application/domain): `[streams]`
  section, startup validation, checksum participation (a streams edit = rules edit →
  resnapshot, `transport.rs:873-876`).
- [x] `wire.rs` (nostos-infra): `SubscribeStream`/`UnsubscribeStream` variants,
  `stream_error` encoder, optional `stream` on snapshot boundaries. Round-trip tests.
- [x] `ports.rs` (nostos-application): `SnapshotSource::snapshot_stream(...)`.
- [x] `snapshot_source.rs` (nostos-infra, `pg`): parameterized `WHERE` builder; `$n`
  binds ONLY; tenant clause appended from the principal; ident regex unchanged.
- [x] `transport.rs` (nostos-infra): `build_predicate` takes a pre-bound expr; mid-
  session routing for both new frames; per-stream session register/remove; stream
  bookkeeping on `SocketSubscriptions` (:794-801); cap accounting.
- [x] `main.rs` (nostos-server): load `[streams]`, plumb into `SyncRouterState`.
- [x] `client.rs` (nostos-client): session-task command mpsc; `sync_stream` +
  `StreamHandle::unsubscribe`; reconnect re-subscribe.
- [x] `sdk/nostos_flutter` (rust/api + lib/src/nostos.dart): frb subscribe/unsubscribe;
  Dart `syncStream(name, params).subscribe()`.
- [x] e2e: `crates/nostos-infra/tests/e2e_pg_sync_streams.rs` — §6 items 1-6.
- [x] Docs: ADR (next free number) recording Decisions 2-4; parity-plan P5 row flip;
  ponytail comments at the JOIN ceiling and per-stream resume.
