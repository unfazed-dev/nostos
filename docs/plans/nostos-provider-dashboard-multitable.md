# Nostos Provider Dashboard — Multi-Table Offline-First Demo

Status: **proposed** (awaiting operator sign-off on schema + phasing). Supersedes the
single-table Tasks demo (`sdk/nostos_flutter/example`). Author: tech lead. Date: 2026-07-15.

## Goal

Replace the single-table Tasks example with a **multi-table offline-first Provider
Dashboard** (booking + ecommerce) running as the macOS desktop example. It must
demonstrate, end-to-end, that nostos is a genuine local-first sync engine:

- **Fully usable during real network failure** — reads + writes hit on-device SQLite;
  the `/sync` WebSocket is a background peer, never a write-gate.
- **Auto retry / resume / refresh** when the link returns — durable outbox flushes,
  live updates stream back in.
- **Multiple tables per handle** — `appointments`, `providers`, `clients`, `invoices`,
  `availabilities` synced + reactive in one app.
- **A real `disconnect()`/`connect()`** control (WS5 Pause/Resume made real) — a true
  stop/start of the sync loop, not a "simulate", keeping the local store usable.

This is the app that proves nostos's multi-table offline-first sync on a real vertical, not a toy.

## Why now / what broke

The Tasks demo's operator buttons (Disconnect / Stop / Airplane) were "all identical"
and the Airplane was theater. Two
hasty rewrites made it worse: one blocked writes on disconnect (anti-offline-first),
one faked an outage with a dead endpoint ("Simulate outage"). Root cause verified this
session: **nostos's FFI exposes only `close()`**, which drops the whole `Session`
(incl. `SqliteStorage`) — so writes/reads die. There is no primitive to "drop the `/sync`
WS but keep the store usable", and no multi-table-per-handle. This plan builds both,
nostos-native.

## Research (grounding)

- **Domain model** (booking/appointment, standard): `providers` 1→N
  `appointments`/`availabilities`; `clients` 1→N `appointments`/`invoices`;
  `appointments` N→1 provider+client; `invoices` 1→1 appointment. Sources: Redgate
  appointment data model; Medium booking-system architecture (PostgreSQL).
- **Reference sync-rules design** (a comparable engine): multi-table via YAML Sync Rules
  (bucketed, parameterized by user) + per-table `watch()` on local SQLite; chat tutorial
  (users/channels/messages) is canonical. **nostos differs**: `where_sql` safe-SQL per
  subscription, collapsed server-gated write-back over one `/sync` WS — not sync-rules.
- **nostos architecture map** (verified across all 5 layers this session): the **data
  plane is already fully multi-table-capable** — `RowOp.table` (`nostos-domain/events.rs`),
  `WireFrame.table` (`nostos-infra/wire.rs:20-30`), `cairn_data(table_name, pk)`
  (`nostos-client/sqlite.rs:53-58`), `Storage::apply_batch` (per-op slice),
  `apply_schema` (one view/table), `FanOutService` + table-indexed `SessionStore`.
  Only **5 narrow points** enforce one-table-per-handle (see D1). **No `nostos-core`/
  data change required.**

## Design decisions

### D1 — Multi-table via a single multiplexed `/sync` WS (deeper; industry-standard)

Industry research (2026-07-15) **overturned the glue-only default**. Among nostos's direct
architectural peers (PG logical replication → server → on-device SQLite, server-gated
write-back, safe-SQL partial subscribe) the pattern is **single-multiplexed-stream,
without exception**: a comparable bucket-based sync engine (sync rules/buckets over one WS), WatermelonDB ("sync is
performed for the entire database at once, not per-collection"), Replicache (one pull/push
over one store), Triplit (one WS). The glue-only N-clients-per-handle alternative matches
only ElectricSQL — a weak analog (HTTP long-poll, no native write-back, no cross-table
transactions).

Three reasons the single-WS path wins for nostos:
1. **Cross-table consistency** — a booking write (`appointment` → `provider` + `client`)
   needs all three tables to reach one consistent point. One stream gives a single
   resume/checkpoint spanning all tables; N sockets each carry an independent LSN with no
   consistent cross-table point (a comparable engine's checkpoint-complete frame spans all
   buckets — only possible on one stream).
2. **Connection/accounting cost** — N tables × M clients = N×M server connections (glue)
   vs M (single-WS). Undercuts nostos's throughput/scalability pitch (142k ops/s).
3. **One auth / one resume / one write-ack stream** — one TLS+auth handshake, one LSN
   resume token, writes ack'd through the same stream.

**Implementation (LOCKED 2026-07-15 — architecture advisor, HIGH confidence, two
consults): DROP the wire-version gate.** Primary-source refutation of the gate: ADR-0009
acks **per-session-sink** (`Ack{lsn}` stamps `sink.acked_lsn`; `min_acked_lsn` folds across
*sessions = sockets*, not tables); the client holds **one global checkpoint**
(`nostos_checkpoint`; `Storage::apply_batch` advances one; `client.rs` sends one
`Ack{lsn = checkpoint}`); there is **one logical-replication WAL feed with in-order
apply**, tables demuxed by `WireFrame.table` *within* it — a per-table LSN **does not
structurally exist**. So multi-table-on-one-socket stays **one session / one sink / one
`acked_lsn`**, identical to today's single-table. The "shared-fate / slow-table-blocks-
others" risk the advisor first raised is structurally impossible here, so the gate is dead
weight. Concretely:

1. `transport.rs:531` — flip ignore → **register-if-absent** (a 2nd `Subscribe` for a *new*
   table registers an additional `SyncSession` on the socket; a repeat for an
   already-subscribed table is an idempotent no-op). **No `Subscribe` message-shape change**
   (`wire.rs:46` stays stable; no version negotiation).
2. **Per-socket table cap = 32** (documented; pending load-test) — bounds snapshot-on-
   subscribe (`transport.rs:298`) cost O(tables); a `Subscribe` beyond the cap closes the
   socket with a clear reason (DoS guard).
3. `client.rs:70` — `SyncClientConfig.table: String` → `tables: Vec<TableSub>` (name +
   optional `where_sql`); `run_once` sends N `Subscribe` frames (the same socket-wide
   `resume_lsn`); the receive loop already demuxes by `WireFrame.table` and applies in WAL
   order — **one checkpoint, one `Ack`, unchanged** (ADR-0009).
4. FFI `Session` → multi-table client + per-table row sinks; `subscribe()` **adds** (not
   replaces); `write` accepts any subscribed table. Dart `_subscribedTable` → a set.

**Safeguards (the advisor's three conditions, all in):** (i) the cap above; (ii) a
backward-compat test proving an old single-Subscribe client still works once
ignore→register flips (register-if-absent makes a stray repeat a no-op); (iii) ADR-0022
records the no-gate deviation + this ADR-0009 analysis.

Best practices adopted regardless: per-table backpressure inside the WS (per-table op-IDs
+ flow control — does NOT need N sockets), one upload path + write-checkpoint ack,
per-table checksums, on-demand partial subscribe via `where_sql` (nostos already has this),
per-table authz in `where_sql`.

Enforcement points lifted by D1: `transport.rs:213/268/531` (N sessions/socket),
`client.rs:70/504` (multi-table client), FFI `Session` (multi-table). `wire.rs:46`
`Subscribe` stays single-table-per-frame; D1 uses repeated frames, not a widened message.

### D2 — `disconnect()`/`connect()` = WS5 Pause/Resume, made real

Implements the already-designed WS5 Pause/Resume semantics:

- `disconnect()` — abort **only** the `run_task`(s) (the `/sync` loop(s)). Keep
  `client` + `pump_task` + `SqliteStorage` alive → reads/writes/UI keep working; writes
  land in the durable `cairn_outbox`; aggregate badge → `disconnected`; **no retry**.
- `connect()` — re-spawn the `run_task`(s) (`run_connection_loop`, same WS, same
  collapsed write-back) → reconnect → flush outbox → live updates resume; badge →
  `connecting` → `connected`.

FFI change: `Session` stores `config` + `state_sink` (today consumed by the spawn) and a
reassignable `run_task` so `connect()` can respawn. With D1, this loops over all
subscriptions. `close()` stays (full teardown). `Drop` aborts all tasks.

### D3 — Writes are NEVER gated on connection state

The offline-first core. `write()` succeeds whenever the per-table `Session` exists
(`subscribe()` called), regardless of WS state — it writes the local SQLite outbox; the
client flushes on (re)connect (ADR-0013, `chaos_write_resume.rs`). The demo removes every
`_held`/`_db==null`-on-disconnect write guard; the network is an observed badge, not a
write-gate.

### D4 — Schema (single-tenant v1)

Postgres tables (uuid pk everywhere — nostos write-back binds pk as `$1`):

| table | columns |
|---|---|
| `providers` | `id uuid pk, name text, specialty text, email text, phone text, created_at timestamptz` |
| `clients` | `id uuid pk, name text, email text, phone text, notes text, created_at timestamptz` |
| `availabilities` | `id uuid pk, provider_id uuid, weekday int (0–6), start_min int, end_min int` (recurring weekly slots) |
| `appointments` | `id uuid pk, provider_id uuid, client_id uuid, starts_at timestamptz, duration_min int, status text, notes text, created_at timestamptz` |
| `invoices` | `id uuid pk, appointment_id uuid, client_id uuid, amount_cents int, status text, issued_at timestamptz, created_at timestamptz` |

- **Single-tenant v1**: all rows sync (no `where_sql`). Ponytail: per-provider
  `where_sql` partitioning for multi-tenant.
- **Availabilities = recurring weekly** (weekday + minute offsets). Ponytail: dated
  exception/override slots.
- Seed data: a few providers, clients, availabilities; appointments/invoices created at
  runtime via the dashboard.

### D5 — Sharpened generic API (zero backend code, no upload connector)

Applies to Flutter (reference impl) and **all SDKs** as a generic contract. Consulted
(GLM-5.2, HIGH). **Headline differentiator: "Zero backend code."** nostos's collapsed
read/write (`PgWriteBack`) means **no dev connector / `uploadData`** — the moat competing
sync SDKs force every user to build. Making that invisible IS the product. Typed records are a
*reason*; cursor-incremental-resume is *plumbing*; users buy the promise.

**One class: `Nostos`** (unify `Nostos` + `NostosDatabase`; `NostosDatabase` → thin deprecated
alias, hard-deprecate post-1.0). Progressive disclosure — simple by default, full power
always one named-param away:

```dart
// L0 — happy path. One line, full power, multi-table, offline-first.
final db = await Nostos.connect(url);
db.watch('appointments').listen((rows) => …);
await db.write('appointments', op: WriteOp.upsert, pk: id, row: {...});

// L1 — the ONE simple knob: a single-concern sync-strategy enum. It selects strategy +
//    sensible defaults ONLY — every capability (multi-table, writes, disconnect/connect,
//    typed reads) is available in all modes. (Not a capability gate, per consultant.)
final db = await Nostos.connect(url, syncMode: SyncMode.localFirst);   // default
enum SyncMode { localFirst, onlineFirst }
final db = await Nostos.connect(url, syncEnabled: false);              // read-only cache

// L2 — flexible: explicit schema, per-table where (partial sync / authz), auth.
final db = await Nostos.connect(url,
  schema: Schema([Table('appointments', […]), …]),
  where: {'appointments': 'provider_id = :uid'},
  token: jwt,
);
final db = await Nostos.supabase(url, accessToken: token);

// L3 — power: NostosConfig + lifecycle + conflict hooks.
final db = await Nostos.connect(url, config: NostosConfig(
  backoff: NostosBackoff.exponential(max: Duration(seconds: 30)),
  deadLetter: DeadLetterPolicy.quarantine(after: 10),
  uploadConnector: MyValidator(),      // opt-in write interceptor (parity P4)
  onConflict: (c) => ConflictResolution.serverWins,   // VISIBLE conflict policy
));
await db.disconnect(); await db.connect(); await db.close();

// Surface (generic single + multi table — schema cardinality is the only axis):
Stream<List<Map>>   watch(String table, {String? where, Duration? throttle});
Stream<List<T>>     watchOf<T>(String table, T fromRow(Map), {String? where});
Future<List<Map>>   getAll(String table, {String? where});
Future<List<Map>>   query(String sql);
Future<void> write(String table, {required WriteOp op, required String pk, Map? row});
Future<void> insert(String table, {required String pk, required Map row});
Future<void> update(String table, {required String pk, required Map patch});
Future<void> delete(String table, {required String pk});
enum WriteOp { upsert, delete, patch }
Stream<NostosConnection> get connection;
Stream<WriteEvent>      get writes;     // acks / conflicts / dead-letters → optimistic UI
```

**Why this design wins:** (1) zero backend code (no connector/`uploadData`);
(2) auto-schema (no explicit schema object required); (3) one simple `SyncMode` enum —
progressive disclosure comparable SDKs lack; (4) generic multi-table by default via `where_sql`
per table, no server YAML sync-rules; (5) visible conflict policy + `writes` event stream
(comparable SDKs are silent last-write-wins); (6) typed reads without codegen (`watchOf<T>`),
Rust perf, cursor incremental resume, Apache-2.0.

**Generic single + multi table:** a connection holds N tables (schema — auto-fetched or
explicit); single-table is N=1. `watch(table)` / `write(table, …)` are identical either
way. No single-vs-multi "modes" — explicit modes would be an artificial API tax.

**Followups (non-blocking):** validate `syncMode` / `syncEnabled` naming with a few
prospective users before freeze; spec the `onConflict` + `WriteEvent` surface concretely
(ADR-0022 addendum); set a hard deprecation date for the `NostosDatabase` alias.

## Phasing

Each phase is independently shippable + verified. **Operator decision (2026-07-15):
P2–P4 build all 5 tables in one pass** (no 2-table interim slice). The multi-table-depth
choice (D1 glue-only vs deeper) is **pending an industry-best-practices research sweep**
(see Open questions). Order:

- **P1 — Schema.** DDL for the 5 tables in the docker Postgres; add to `cairn_pub`
  publication; set `NOSTOS_WRITE_TABLES=appointments,providers,clients,invoices,availabilities`;
  confirm `GET /schema` returns all 5 (it already discovers multi-table schema). Seed.
  *Files:* `docker/pg-init/*.sql`, server launch env, `Makefile` dev-stack.
- **P2 — Multi-table SDK (D1 LOCKED: drop-the-gate).** Server: `transport.rs:531`
  ignore→register-if-absent (N `SyncSession`s/socket) + per-socket table cap 32. Client:
  `SyncClientConfig.tables: Vec<TableSub>`; `run_once` sends N Subscribes (one socket-wide
  `resume_lsn`); one checkpoint/Ack unchanged (ADR-0009). FFI: `Session` multi-table +
  per-table row sinks; `subscribe()` adds; `write`/`watch`/`query` route by table;
  aggregate `connectionState`. Backward-compat test (old single-Subscribe client).
  `flutter_rust_bridge_codegen` regen; Dart `Nostos`/`NostosDatabase` multi-table API.
  *Files:* `nostos-infra/src/transport.rs`, `nostos-client/src/client.rs`,
  `sdk/nostos_flutter/rust/src/api/nostos.rs` + Dart (`nostos.dart`, `nostos_database.dart`,
  `engine.dart`). (`wire.rs` UNCHANGED — no gate, no message-shape change.)
- **P3 — `disconnect()`/`connect()` (D2).** FFI abort/respawn `run_task`(s), keep
  clients+storage; Dart wrappers. *Files:* same FFI + Dart surface.
- **P4 — Provider Dashboard app (D3).** Replace `example/lib/main.dart` Tasks app:
  NavigationRail (Appointments / Providers / Clients / Availabilities / Invoices);
  reactive `watch()` per table; create/complete/cancel appointment; create invoice;
  **Disconnect/Connect** toggle (real, D2); aggregate connection badge; offline-write
  counter. macOS desktop target.
- **P5 — Smoke-test + verify.** Live 5-table sync; **Disconnect → add appointments +
  invoices offline (queue) → Connect → flush + live echo**; kill server (real cut) →
  app usable → restart → auto-resume. `flutter analyze` clean; the
  `chaos_write_resume` contract holds per-table.

## Verification (Gate-4 bar)

- `flutter analyze` clean on the example.
- Live: all 5 tables render reactively; cross-table writes echo back.
- Offline-first: Disconnect → writes still land locally + queue; Connect → flush + live.
- Real cut: kill `nostos-server` mid-session → app usable → restart → auto-resume/flush.
- Per-table outbox durability survives disconnect/connect (ADR-0013, `chaos_write_resume`).

## ADR-0022 — Multi-table-per-handle + disconnect/connect

Records: D1 (N per-table clients, glue-only; the deferred single-WS path), D2
(WS5 Pause/Resume semantics), D3 (writes never gated), and the 5 untouched data-plane
facts that make it cheap. Cites this plan + the architecture map.

## Open questions (need operator sign-off before P2+)

1. **Schema** — columns above OK? Recurring weekly availabilities vs dated slots?
   Single-tenant (all rows) vs per-provider `where_sql` partitioning for v1?
2. ~~Phasing — minimal slice vs all-5.~~ **Resolved 2026-07-15: all 5 tables in one pass.**
3. **Multi-table depth** — ~~glue-only vs deeper~~ **RESOLVED 2026-07-15**: (B) deeper
   single-multiplexed-WS (industry-standard among WatermelonDB/Replicache/Triplit and
   comparable bucket-based sync engines).
   **Wire-gate question RESOLVED 2026-07-15 (advisor, HIGH, two consults): DROP the gate** —
   ADR-0009 is per-session-sink with one global checkpoint and a per-table LSN does not
   structurally exist. D1 + P2 updated. Operator gave go on all three tracks (dashboard →
   CRDT → flagship), staged in dependency order.
