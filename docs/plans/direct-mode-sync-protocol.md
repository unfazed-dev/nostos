# Direct mode — the sync protocol, and how the device gets transactional consistency

**Date:** 2026-09-22. **Status:** all nine steps implemented and under
`make ci`, and the whole protocol now runs against a REAL Supabase stack —
PostgREST, Realtime, GoTrue and the Edge Runtime, not stubs. Grounded in
fetched docs throughout.

Shipped: `nostos_core::pull` (`PullCursor`, `Horizon`, the xid8 checkpoint on
`Storage`), `nostos_client::postgrest` (`rpc/nostos_pull` + the four write ops),
`nostos_client::doorbell` (Realtime private channel, `vsn=1.0.0`), and
`nostos_cli::direct` — the generator behind `nostos link --mode direct` and the
verifier behind `nostos doctor --mode direct` — plus `nostos_core::conformance`
(one suite, run per platform) and the opt-in push path.

**The SQL below is no longer theory.** `crates/nostos-cli/tests/e2e_pg_direct_sql.rs`
applies the generated file to a real Postgres and asserts the properties that
cannot be tested in Rust: an in-flight transaction hides every later commit,
a page never splits a transaction, RLS scopes the log to the caller's claims,
`nostos_increment` is atomic, and a row that changes owner is logged as a delete
under the old scope. It runs under `make pg-e2e`.

**And the Supabase-specific half is no longer theory either.**
`scripts/e2e-supabase-direct.mjs` drives the same generated SQL through a real
Supabase stack (`supabase start`) — 30 checks, green — and settles the W0 list:
PostgREST emits `xid8` as a JSON **string** (a `number` would round the cursor
past 2^53), `raise sqlstate 'PT410'` really does arrive as HTTP 410 with
`code: "PT410"`, `auth.jwt()` inside `nostos.current_scopes()` sees real GoTrue
claims, and the generated policy on `realtime.messages` both delivers the ring
to its own tenant and refuses another one by name. It found four bugs no amount
of reasoning had — see "What the live stack found" below.
**Companion to:** `nostos-on-device-no-always-on-server.md` (*why* direct mode).
**Supersedes:** this file's first draft (per-table `updated_at` watermarks),
which conceded that cross-table transactional consistency was impossible
without a sync server. **It isn't.** The concession was an artefact of the wrong
change-source design, and the operator was right to refuse it.

Direct mode = the device syncs from the client's own Postgres via PostgREST +
Realtime. No Nostos server, no second box, every sync decision made on the
device.

## The design: a change log in the client's own database

One append-only table in a `nostos` schema. A trigger on each synced table
appends to it **inside the writing transaction** — the transactional outbox
pattern, whose entire purpose is to make the data change and the change
notification atomic without two-phase commit.

```sql
create table nostos.changes (
  seq        bigserial primary key,
  xid        xid8        not null default pg_current_xact_id(),
  table_name text        not null,
  pk         text        not null,
  op         text        not null,          -- insert | update | delete
  row        jsonb,                          -- full row image; null on delete
  scope      text                            -- tenant/owner, stamped by trigger
);
create index on nostos.changes (xid, seq);
```

Everything below follows from two properties of that table: **one sequence
across every table**, and **a transaction ID on every row**.

## Why this gives cross-table transactional consistency

`pg_current_xact_id()` returns the writing transaction's ID, typed `xid8` — and
the PostgreSQL docs are explicit that `xid8` "does not wrap around during the
life of an installation", so it is a permanent monotonic identity, not a
recycled counter.

So every change row carries the transaction that produced it, across all
tables. The device groups a pull by `xid` and applies each group in **one SQLite
transaction**. A transaction that touched three tables lands as three tables'
worth of rows or none of them.

That is the same property comparable sync engines' server-side checkpoints
provide — "only fully committed transactions are part of the state… different
tables and buckets are all included in the same consistent checkpoint" —
obtained here from Postgres itself rather than from a service. Nostos's own `ReplicationEvent`
already carries `txn_id` (`crates/nostos-domain/src/events.rs`), and
`ApplyEngine` already applies at commit boundaries, so the client-side half of
this **already exists**; only the frame source changes.

## Why it cannot lose a row: the snapshot horizon

The hazard that kills naive change logs: `seq` is assigned when the row is
*inserted*, but the row becomes visible when its transaction *commits*. A
transaction that grabs `seq = 100` and commits five seconds after one that
grabbed `seq = 101` will appear *below* a watermark that has already advanced
past it. The row is then never read again. Silent, permanent loss.

Postgres exports the fix, and the docs name this exact use case: the transaction
ID and snapshot functions exist "to determine **which transactions were
committed between two snapshots**."

`pg_current_snapshot()` returns `xmin:xmax:xip_list`, and
`pg_snapshot_xmin(...)` is the lowest transaction ID **still in progress**.
Every `xid` below it is finished — committed (so its log rows are visible) or
aborted (so its log rows never existed). Nothing new can ever appear below it.

**That makes the horizon a safe, monotonic, gapless checkpoint.** The device
stores one `xid8`, not a timestamp per table:

```sql
create function public.nostos_pull(since xid8, max_txns int default 200)
returns jsonb   -- ONE value, not a set: see "Why a scalar" below
language sql stable security invoker as $$
  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon),
  page as (
    select distinct c.xid
    from nostos.changes c, h
    where c.xid >= since and c.xid < h.horizon
    order by c.xid
    limit greatest(max_txns, 2)
  )
  select h.horizon, c.seq, c.xid, c.table_name, c.pk, c.op, c.row
  from nostos.changes c
  join page p on p.xid = c.xid
  cross join h
  order by c.xid, c.seq;
$$;
```

**Why a scalar.** PostgREST caps a set-returning response at `db-max-rows`
— 1000 on a stock Supabase project — and announces the cut with nothing but a
`content-range: 0-999/*` header on a `200 OK`. No 206, no error, and `Range`
and `offset` are both ignored on an RPC, so there is no paging past it either.
A page of `max_txns` transactions has no row bound at all, so the tail would be
dropped and the device would then store a horizon *past rows it never saw* —
silent, permanent loss, and invisible from the client. Returning one `jsonb`
value makes the response one row however big it gets, so the cap cannot reach
it; PostgREST renders a jsonb scalar as the bare array the device already
parses, so the wire shape is unchanged. Measured 2026-09-22 against a real
project: a 1010-row snapshot came back as 1000 rows, `200 OK`, with two whole
tables missing from the payload.

**The page is `max_txns` transactions, not `max_rows` rows — and that is
load-bearing, not a preference.** A row limit knows nothing about transaction
boundaries, so it can cut one in half, and the client then has to hold the
partial tail back and re-read it. That combination livelocks: `since` must be
*inclusive* (the horizon's own transaction is still in flight and its rows
arrive later), so a re-read returns the same rows, fills the page again, and
truncates the same tail forever. Paging by transaction dissolves it — every page
is whole transactions, the client truncates nothing, and `greatest(max_txns, 2)`
guarantees a full page spans at least two transactions so the cursor always
advances. Rows per page is then bounded only by the largest transaction in it,
which no pagination scheme can fix: an atomic apply has to hold the whole
transaction anyway.

Three things fall out of this being **one function call**:

1. **The snapshot and the rows come from one transaction**, so the checkpoint is
   marked synchronously with the query. WatermelonDB's backend contract demands
   exactly this — "perform all queries synchronously or in a write lock… to
   ensure that no changes are made to the database while you're fetching
   changes (otherwise **some records would never be returned in a pull
   query**)" — and offers a lossy duplicate-tolerant fallback for backends that
   can't. A single Postgres function does not need the fallback.
2. **`security invoker`** means RLS applies as the calling user. Scoping stays
   in Postgres, i.e. still server-authoritative.
3. **It lives in `public` under a `nostos_` prefix, not in the `nostos` schema.**
   Supabase exposes `public, graphql_public` by default; a third schema needs a
   `Content-Profile` header on every request *and* an operator ticking it into
   "Exposed schemas". Prefixing deletes both steps — and keeps the log table
   off the REST API entirely, so there is no `GET /rest/v1/changes` whose
   grants could be got wrong. The device posts to `/rest/v1/rpc/nostos_pull`.

## What this dissolves from the first draft

| first draft's "non-negotiable" rule | status now |
|---|---|
| composite `(updated_at, pk)` checkpoint | **gone** — one `xid8` horizon |
| `updated_at` trigger on every synced table | **gone** — no clock involved anywhere |
| soft delete mandatory on user tables | **gone** — the log records `op = delete` |
| watermark-before-query, tolerate duplicates | **gone** — one RPC, one snapshot |
| no cross-table transactional consistency | **gone** — group by `xid` |

WatermelonDB's own tips anticipate this. After describing the timestamp approach
they warn it needs a stored procedure enforcing "uniqueness and monotonicity" to
protect "against weird edge cases — such as records being lost due to server
clock time changes (NTP time sync, leap seconds, etc.)", then name the
alternative: "an auto-incrementing counter sequence, but you must ensure that
this sequence is **consistent across all collections**." A single change log is
that sequence, consistent across all collections by construction.

## What survives

**The realtime stream is a doorbell, and every reconnect pulls.** A second
trigger on `nostos.changes` calls `realtime.broadcast_changes()` on a
scope-keyed private channel. A message means "call `pull`", nothing more. RxDB
states the rule: "when the client goes offline and online again, it might happen
that `pullStream$` has missed out some events. Therefore `pullStream$` should
also emit a RESYNC event each time the client reconnects." Nostos already holds
the same position — push is a "wake-up trigger, not a data channel"
(`docs/STRATEGY.md:214`, ADR-0037). This also makes the ~3-day
`realtime.messages` retention a non-issue: a device away for a month takes the
same code path as one that blinked.

**Private channels need the setting, not just the policy.** Supabase: access is
controlled "by adding Row Level Security policies to the `realtime.messages`
table", and "to enforce private channels you need to **disable the 'Allow public
access' setting** in Realtime Settings". `nostos doctor` checks the setting.

## The honest costs — five, and none of them is a correctness hole

1. **Write amplification.** Every change to a synced table writes a second row,
   in the same transaction, so writes get slower and the database grows. This is
   the price of the outbox pattern and it is the main reason to prefer logical
   replication when you *can* run a server.
2. **RLS has to be expressible on one table.** The log holds row images from
   many tables, so one policy on `nostos.changes` must say what N table policies
   say. The `scope` column stamped by the trigger covers the common
   tenant/owner case. A client whose RLS involves joins across tables is real
   work, and `nostos link` should refuse rather than guess.
3. **A long write transaction delays everyone.** The horizon cannot pass an
   in-flight write, so a 30-second transaction holds sync back 30 seconds.
   Logical replication has the identical property — nothing can be emitted
   before commit — so this is not a regression, but it is worth knowing.
4. **Retention and re-snapshot.** Prune the log on a window; a device that was
   away longer re-snapshots the tables through PostgREST and resets its horizon.
   Needs the same "read the horizon first, then the rows" discipline.
5. **`track_commit_timestamp` is deliberately not used.** It would give real
   commit times via `pg_xact_commit_timestamp`, but the docs say it only works
   "for transactions that were committed after it was enabled" and that "commit
   timestamp information is routinely removed during vacuum". The snapshot
   horizon needs no server setting and no vacuum-sensitive data.

## Implementation order

1. ✅ `ChangeSource` seam as **pure functions in `nostos-core`**, not in
   `nostos-client` — see "Every SDK gets this" below for why. `nostos-client`
   and `nostos-ffi-wasm` each supply the I/O; `iroh_dial.rs` is the precedent
   for dial-by-scheme at the native edge.
2. ✅ `xid8` horizon in `nostos_meta` beside the existing LSN checkpoint, carried
   as an opaque string.
3. ✅ `PostgrestChangeSource`: `rpc/pull` → group by `xid` → `RowOp` batches
   handed to the existing `ApplyEngine` at transaction boundaries.
4. ✅ Realtime private-channel subscription as doorbell; pull on every reconnect.
5. ✅ Outbox drain → PostgREST write; the log trigger captures the echo, which the
   idempotent `(table_name, pk)` store absorbs. **`WriteOp::Increment` needs a
   second generated function** — see "The write path needs one more function"
   below.
6. ✅ `nostos link --mode direct` generates the schema, the per-table triggers,
   `nostos_pull`, `nostos_increment`, the broadcast trigger, the RLS policies and
   the grants — and refuses any table whose RLS it cannot express as a `scope`.
   `crates/nostos-cli/src/direct.rs`; applied to real Postgres by
   `crates/nostos-cli/tests/e2e_pg_direct_sql.rs`. See "What the generator
   refuses" below.
7. ✅ `nostos doctor --mode direct`: the objects, the grants (including that
   `anon` may NOT execute `nostos_pull`), read-only policies, the per-table
   triggers, the Realtime policy, log growth and horizon lag —
   `nostos_cli::direct::inspect`, all `select`s, safe against production.
   **Its load-bearing check is that the deployed `nostos_pull` pages by
   transaction**: a row-limited one still returns rows and still advances a
   horizon, so it looks healthy from the device while handing out half a
   transaction. No client-side test can see that, so doctor reads the deployed
   function's own source. The e2e deploys the bug on purpose and asserts the
   check catches it. The "Allow public access" switch is reported as a note —
   SQL cannot see it.
8. 🟡 One conformance suite both modes pass, **run per platform, not once** —
   `nostos_core::conformance` (feature-gated, four cases) is the suite;
   `nostos-core`'s own test runs it on `InMemoryStorage` and
   `crates/nostos-client/tests/conformance_sqlite.rs` runs it on rusqlite.
   **The browser-Worker leg is not wired up** — `nostos-ffi-wasm` has to export
   `run_all` first, and the Dart harness has to call it through the bridge.
   Retention is now a real case end to end: `nostos.prune()` records how far it
   pruned, `nostos_pull` raises `PT410` for a horizon below that (PostgREST →
   HTTP 410), and the client surfaces `PostgrestError::Gone` rather than an
   empty page. The original text: —
   `apps/atlet/flutter/test/adapter_conformance_test.dart` is the existing
   Dart-side harness (see "The test bed already exists" below) —
   including **"transaction touching three tables is never seen
   half-applied"** and **"device offline past the retention window"** as
   first-class cases. The first of those has to hold inside the browser
   Worker as well as on rusqlite.
9. ✅ Push, opt-in behind `nostos link --mode direct --push <url>`:
   `nostos.push_tokens` + `nostos.device_presence` + `nostos.push_cooldown`,
   `nostos_register_push_token` / `nostos_deregister_push_token` /
   `nostos_heartbeat` (all `security definer`, all taking the scope from the
   caller's own JWT so a device cannot register against another tenant), the
   `nostos.wake_absent_devices()` trigger → `pg_net` → Edge Function, and the
   matching `PostgrestSource` methods. The reference function is
   `supabase/functions/nostos-push/index.ts` (data-only FCM v1, shared-secret
   bearer, `--no-verify-jwt`). `--push` writes the function into the app repo,
   `--visible` adds templated alert pushes (ADR-0037 §2b), and `--deploy
   --fcm-service-account <json>` rolls all of it out through the `supabase`
   CLI. Firebase/APNs setup is still the operator's.

   Two findings from building it:

   - **The debounce cannot take a row lock.** One shared cooldown row per scope
     is a row lock per scope held until the writing transaction commits, so a
     long transaction would block *every other writer in that scope*. A delayed
     sync is acceptable; a blocked write is not. `pg_try_advisory_xact_lock`
     goes in front: it never waits, and a writer that finds the scope taken
     skips — which is what the debounce would have told it anyway. **The pg e2e
     found this by deadlocking.**
   - That same debounce is the answer to the open `pg_net` rate question: five
     writes to one sleeping scope are one request, not five.

### The write path needs one more function than `pull`

Three of the four outbox ops are plain PostgREST:

| `WriteOp` | request |
|---|---|
| `Upsert` | `POST /<table>`, `Prefer: resolution=merge-duplicates` |
| `Patch` | `PATCH /<table>?id=eq.<pk>` — never inserts, matching the op's contract |
| `Delete` | `DELETE /<table>?id=eq.<pk>` — zero rows matched is success |

`Increment` is not, and it cannot be made to be. ADR-0030's guarantee is that
**Postgres** serializes concurrent increments (`SET x = x + ?`), which is what
removes the client read-modify-write and therefore the lost update. A PATCH body
carries literals, so expressing an increment through one puts the read back on
the device. Direct mode therefore needs `nostos_increment(p_table, p_pk, p_field,
p_delta)` alongside `pull` — two generated functions, not one.

The echo needs no handling. A write fires the change-log trigger, the device
pulls its own row back, and the idempotent `(table, pk)` upsert absorbs it. No
client-id round trip, no suppression list.

RLS on the target table is the *only* thing authorizing a direct-mode write.
That is the security argument rather than a caveat: a forbidden write is refused
by Postgres, not by a service the developer has to trust — and a `403` is
therefore permanent (dead-letter it), while a `401` is an expired JWT (refresh
and retry).

### What the generator refuses, and the two things it had to decide

`nostos link --mode direct` reads `nostos_rules.toml` and emits one re-runnable
file. Cost #2 above — "RLS has to be expressible on one table" — is now
executable: a rule is expressible only when it is exactly
`<column> = claims.<field>`. Everything else is refused **by name**, with the
reason, rather than generating a policy that quietly shows the wrong rows:

| rule | refused because |
|---|---|
| `org_id = claims.org AND status = 'open'` | two comparisons, one column |
| `priority > claims.min` | only `=` can be answered by comparing one stamped value |
| `status = 'open'` | a literal filters rows rather than scoping them — a row that stops matching would never be sent as a removal |
| *(no scope)* | in server mode that means "tenant-scoped by the session"; direct mode has no session, so it would be readable by every device. Opt in out loud with `--public <table>` |
| `[streams.*]` present | a stream is a server-held predicate template; there is no server to hold it |

Two decisions the generator had to make, neither of which the plan had settled:

1. **Scope values are namespaced `<claim>:<value>`.** One shared column holds
   scopes derived from different claims, so `sub` and `org_id` values must not
   be able to collide. `nostos.current_scopes()` returns the caller's namespaced
   set and the policy is one `= any(...)`.
2. **A row that changes scope logs two records** — a delete under the old scope
   and the update under the new. Without it the losing tenant keeps the row on
   device forever, because a row they can no longer see can never be sent to
   them again.

## Every SDK gets this, because it needs only two primitives

Direct mode's entire client-side dependency is **one HTTPS POST** (`rpc/pull`)
and **one WebSocket** (the doorbell). Server mode already requires the
WebSocket, so direct mode adds exactly one capability — an HTTP POST — and no
platform Nostos ships to lacks it. **Direct mode is the more portable of the two
modes, not the less.**

It reuses the platform split that already exists:

| SDKs | how it reaches Rust | pull + doorbell I/O | storage (unchanged) |
|---|---|---|---|
| `nostos_flutter` (native), `nostos_tauri`, `nostos_node`, `nostos_swift`, `nostos_kotlin`, `nostos_dotnet` | `nostos-client` directly (each has its own `Cargo.toml`) | tokio HTTP + WS | `SqliteStorage` (rusqlite) |
| `nostos_react_native` | TurboModule over the `nostos_swift` / `nostos_kotlin` UniFFI bindings — **no Rust crate of its own** (ADR-0020) | inherited from those two | inherited |
| `nostos_web`, `nostos_capacitor`, `nostos_flutter` on web (ADR-0036) | `nostos-ffi-wasm` `--target web`, inside the Worker | JS `fetch` + `WebSocket` | `SqliteWasmStorage` → sqlite-wasm `opfs-sahpool` (ADR-0033) |

Two placements are counter-intuitive and both were checked rather than guessed:

- **`nostos_tauri` is native** despite rendering a web UI —
  `sdk/nostos_tauri/Cargo.toml` depends on `nostos-client`, `nostos-core` and
  `nostos-domain`, so the webview never touches the sync path.
- **`nostos_capacitor` is *not* native.** It has no Cargo.toml;
  `sdk/nostos_capacitor/src/web.ts` loads `pkg-web/nostos_ffi_wasm.js` and drives
  `NostosSocket` inside the WKWebView / Android WebView, on the grounds that both
  browser globals "exist and behave exactly as in a desktop browser". So a
  Capacitor app is a browser target for sync purposes, and direct mode reaches
  it through the wasm row.

The practical consequence: **direct mode needs two implementations, not nine.**
`nostos-client` covers six SDKs and `nostos-ffi-wasm` covers three.

### This is what moves the seam out of `nostos-client`

`nostos-client` is tokio + rusqlite. A seam there covers six SDKs and skips the
three that want it most. The pull logic belongs in **`nostos-core`**, whose own
header states the contract — "**pure Rust: no tokio, no SQLite, no I/O**"
(`crates/nostos-core/src/lib.rs:9`) — and which holds to it: the only matches for
`async fn`, `.await` or `tokio` in `crates/nostos-core/src/` are four doc
comments asserting their own absence.

ADR-0020 is the reason this matters rather than being a tidiness preference. It
settled that React Native **cannot** reuse the JS core, so `nostos-client` and
`nostos-ffi-wasm` are permanently two separate consumers. Logic placed in either
one does not reach the other; logic placed in `nostos-core` reaches both.

This is not a new pattern. `crates/nostos-ffi-wasm/src/transport.rs` already
splits it exactly this way: the frame logic is pure Rust
(`build_subscribe_frame`, `on_message`, `parse_checkpoint`) and the socket
belongs to the platform. Direct mode takes the same shape:

- **`nostos-core`:** `pull_request(since)` and `apply_pull(&mut engine, body)`.
  Pure, sync, host-testable in `make ci`.
- **platform edge:** whoever owns the socket owns the POST — `reqwest` in
  `nostos-client`, `fetch` in the Worker.

ADR-0033 made the same call for storage: `SqliteWasmStorage` lives in
`nostos-ffi-wasm`, "NOT `nostos-core` — core stays WASM-clean". Same rule, same
reason.

### Four web-specific facts

1. **The horizon travels as an opaque string.** The client never does
   arithmetic on it — it stores it and hands it back — so there is no reason to
   route an `xid8` through a JS `number`, where anything past 2⁵³ is silently
   wrong. Note that the existing `Lsn(pub u64)` derives `Serialize` and so goes
   over the wire as a JSON *number*; direct mode should not copy that. What
   PostgREST actually emits for `xid8` is a W0 check against a live project,
   not something to assume.
2. **A doorbell carrying no data is what makes two subscriber stacks
   acceptable.** Web can use `realtime-js`; native opens a raw WS to the
   Realtime endpoint. If the channel carried rows, both paths would have to
   agree on payload decoding. It carries "call `pull`", so they don't.
3. **One puller per app, living in the Worker.** ADR-0033's Worker already owns
   the sole wasm instance and the sole database, with the main thread a pure
   `postMessage` proxy. Put the pull loop there and the horizon advances in one
   place, so multiple tabs cannot race it.
4. **No OPFS means no durable horizon.** Safari Private Browsing degrades to
   `InMemoryStorage`, so every load re-snapshots — the same cost class as
   server mode's snapshot-on-every-reconnect in that configuration, and already
   surfaced on `SyncStatus`.

## The test bed already exists: `apps/atlet/flutter`

Verified 2026-09-22: `flutter test test/` → **107 passed** on Flutter 3.47.5.

Atlet is a Supabase-backed **multi-engine comparison** app, and its shape is the
shape direct mode needs:

- **`lib/adapters/sync_adapter.dart`** — an `abstract interface class SyncAdapter`
  of 17 methods (`watchSessions`, `placeOrder`, `signOut`, `connected`, `marks`…).
  Direct mode is a **third implementation of this interface**, nothing more.
- **`lib/engine_registry.dart`** — an `enum Engine` with a comparison-engine slot
  alongside `nostos`, plus an `EngineRegistry` that hot-swaps them with a
  mutual-exclusion guard (only one adapter may be live at a time). Add
  `Engine.nostosDirect` and the app compares three engines behind one UI.
- **`test/adapter_conformance_test.dart`** (282 lines) — "SyncAdapter
  conformance", already the one-suite-many-engines harness that this plan's step
  8 asks for. It runs against a `FakeAdapter`, so a direct-mode adapter inherits
  the assertions for free.
- **Supabase is already wired.** `supabase_flutter: ^2.9.1`,
  `SUPABASE_URL`/`SUPABASE_ANON_KEY` via `--dart-define` (`lib/main.dart`), and
  four migrations + a seed under `apps/atlet/supabase/`. **The same project that
  unlocks atlet unlocks this plan's W0 gap** — one credential, not two.

`sdk/nostos_flutter/example` is the other Flutter app (a 6-table booking
dashboard on one `/sync` socket). It is the SDK-surface demo, not an engine
comparison: it dials `ws://127.0.0.1:8800` from `nostos dev` and has no adapter
seam, so it exercises direct mode only after step 3 gives it something to dial.

### Atlet's own migrations are the argument for direct mode, in SQL

`apps/atlet/supabase/migrations/0002_replication_publication.sql` originally
created a login role `with replication bypassrls` for a second sync engine
(dropped, repo and live, 2026-09-23).

That is the credential the companion doc calls unshippable — `replication`
**and** `bypassrls`, a key to every row in the database regardless of policy.
It is fine here because only a managed sync cloud holds it. It is exactly what an
agency shipping one shared database to end-user devices cannot put in an APK.
Direct mode ships the anon key and leans on RLS instead.

`0005_replica_identity_full.sql` is the other half: `replica identity full` on
all five tables, because "under the default (PK-only) identity, tenant-scoped
delete fan-out silently drops the event and clients never see the row
disappear." Direct mode does not need it — the change log captures `op = delete`
with the `scope` stamped by the trigger, in the writing transaction, where the
row is still there to read. The migration stays for server mode.

### Eight dead `make` targets — removed 2026-09-22

`fixture-test`, `fixture-e2e`, `fixture-todo-test`, `fixture-todo-smoke`,
`fixture-todo-smoke-live` and `fixture-todo-nostos-live-{up,down,proof}` all `cd`
into `fixtures/flutter/…`, which **`2489ffb` deleted** ("remove superseded
Flutter fixtures (greenfield per plan D0)"). Stripped from the Makefile;
`docs/plans/multi-sdk-pomodoro-fixture-matrix.md` §1 had already flagged all
eight. The todo fixture was the Supabase-live Flutter harness —
`supabase/schema.sql`, `env.example.json`, `nostos_live_{up,down}.sh`,
`integration_test/nostos_live_test.dart` — so its wiring is recoverable from
`2489ffb^` if atlet turns out not to cover a case.

## Worth an ADR

Second sync topology on the public client surface; hard to reverse; a real
trade-off (write amplification and one-table RLS, against no server). That
clears the bar in `.claude/skills/grill-with-docs`. Not written — the decision
is the operator's.

Two decisions, and they are separable. The topology is one ADR. **Flipping
`nostos link`'s default from server to direct is a second**, taken later, on the
evidence of step 8 — see "Which mode is the default" below.

## Which mode is the default

Not a single answer, because direct mode has a hard prerequisite: it talks to
**PostgREST and Realtime**. A plain self-hosted Postgres has neither, so there is
no `rpc/pull` to call and no channel to subscribe to.

| backend | default | why |
|---|---|---|
| Supabase, or anything running PostgREST + Realtime | **direct** | no process for the developer to operate; anon key + RLS is the whole trust story |
| plain Postgres, self-hosted | **server** | direct mode has nothing to talk to |

So the framing is *"direct is the default when the backend can serve it"*, not
*"server mode becomes a flag"*. Server mode stays the answer for four cases
direct cannot serve, and none of them is a deprecation candidate:

1. **Join-based RLS** — `scope` covers tenant/owner; `nostos link` refuses the rest.
2. **Presence-aware push** — see below; it needs `SessionStore`.
3. **Non-Supabase Postgres** — the row above.
4. **The fan-out tier** — one router moving 800k deliveries/sec beats N devices
   each polling their own slice, once N is large enough. Where that crossover
   sits is unmeasured.

**Sequencing.** `nostos link --mode direct` ships behind the flag first — the
ADR-0033 experimental-behind-flag precedent. The default flips only when step 8's
conformance suite is green on rusqlite, inside the browser Worker, and on a
physical iOS device. Inverting a default on a public client surface is the
hard-to-reverse move, and it is the part that earns an ADR.

## Push has to keep working, and it can — without an always-on process

Three things nostos-server does for push today. Each needs a direct-mode answer:

| what the server does | where it lives now | direct-mode replacement |
|---|---|---|
| holds the APNs `.p8` / FCM service-account JSON | `Rails::from_env()`, `crates/nostos-push/src/rail.rs:122` | a Supabase Edge Function secret — **never the device** |
| decides *whom* to doorbell | `FanOutService::fan_out` enqueues one `PushHint` per matched **offline** account, `crates/nostos-application/src/fanout.rs:396` and `:450` | trigger on `nostos.changes` reads `scope`, selects the token rows for that scope |
| receives the device token | `adapter.registerPushToken('fcm', token)` → `POST /push-tokens`, `apps/atlet/flutter/lib/push/push_pilot.dart:179` | an ordinary PostgREST insert into `nostos.push_tokens`, under RLS |

### The one thing that cannot be argued away

An OS-level wake requires APNs or FCM, and both authenticate the sender with a
credential that must never ship in an APK. This is the shared-replication-role
argument again, in a different place: **direct mode means no server the developer
operates, not no server-side code.** The credential holder is an Edge Function —
scale-to-zero, invoked by the same trigger that already fires the Realtime
broadcast, so the atomicity argument at the top of this document covers it too.

`nostos-pushd` is unaffected and stays the answer for an operator who wants a Rust
daemon or is not on Supabase. Direct mode simply does not use ADR-0038's
`RemoteNotifier` delegation.

### What is genuinely lost: presence

`fan_out` doorbells only accounts that are **offline**, and "offline" comes from
`SessionStore`. Direct mode has no session store — nothing is tracking who is
connected. The lazy answer is the right one: **always send the data-only message
and let the client dedupe.** It already has to — the doorbell carries no data and
every reconnect pulls, so a redundant wake costs one `rpc/pull` that returns
zero rows. Add a `last_seen` heartbeat column only if a measurement says the
wasted sends cost something.

### What is not lost: coalescing

`default_collapse_key(tenant, token)` (`crates/nostos-push/src/rail.rs:182`) is a
pure function, and the supersede keys it feeds — FCM `collapse_key`, APNs
`apns-collapse-id`, Web Push `Topic` — are **provider** features, not server
features. An Edge Function setting the same headers with the same key gets the
same behaviour ADR-0038's test-that-matters asserts: 20 sends to one target ⇒
exactly 1 push. The 410/`UNREGISTERED` prune becomes a `delete` on the token
row.

### Atlet already has the whole rail to point at it

`tool/push_smoke.sh` + `integration_test/push_smoke_test.dart` drive real FCM end
to end and assert on both sides (server metric up, device receives the data
message). `web/atlet-push-sw.js` and `lib/push/push_pilot_web.dart` cover the Web
Push leg. Repointing the harness at the trigger-plus-function path reuses the
device-side assertion verbatim — only the "server" assertion changes, from
`nostos_push_sent_total` to the function's own log.

### Researched in full: `direct-mode-push-and-presence.md`

Two of the three W0 checks this section originally raised came back **answered
by the vendor docs, in our favour**:

- **`pg_net` is transactional.** "HTTP requests are not started until the
  transaction is committed", and a `ROLLBACK` discards the queue row. No
  doorbell before the data is visible, none for an aborted write, no 2PC.
- **`realtime.send()` from the trigger is post-commit by construction** —
  Realtime reads the WAL. It is also Supabase's own recommendation over
  `postgres_changes`, which authorizes per subscriber.

The third is still open (**Edge Function cold-start latency**), joined by two
new ones and by the hard platform ceilings: iOS silent push is best-effort and
impossible after a force-quit, and Chrome forbids silent web push. Presence
splits into two jobs — Realtime Presence for the user-visible kind, a
`last_seen` column stamped free by `nostos.pull()` for push suppression.

See `docs/plans/direct-mode-push-and-presence.md` for the ladder, the limits,
and the verification plan.

## What the live stack found

Four bugs, none of which the Rust e2e could see, because each one lives in a
Supabase behaviour that a stub gets right by construction. Run it with
`node scripts/e2e-supabase-direct.mjs` against `supabase start`.

**1. `revoke … from public` does not revoke anything on Supabase.** The
generated SQL revoked EXECUTE from the PUBLIC pseudo-role before granting
narrowly, and the comment claimed that stopped the anon key. It does not:
Supabase's DEFAULT PRIVILEGES grant EXECUTE to `anon`, `authenticated` and
`service_role` **by name** at creation time, and revoking from PUBLIC leaves a
grant made to a named role standing. Every generated function came out with
`anon=X`. For the `security invoker` ones that was only defence in depth — the
table grants still refused the anon key — but the push RPCs are `security
definer`, so it was an anonymous caller registering a push token under the
`public` scope. The revokes now name the roles. `nostos doctor --mode direct`
already asserted "anon may NOT execute public.nostos_pull"; it had simply never
been pointed at a project where default privileges apply.

**2. The Edge Function could never read the token registry.** It used
`createClient(..., { db: { schema: "nostos" } })`, and `nostos` is deliberately
not an exposed schema — the same decision that moved `nostos_pull` into
`public`. Every call returned `500 Invalid schema: nostos`, which reaches
nobody: the push is dropped, `pg_net` records the response in a table the
operator never looks at, and the device just never wakes. Fixed with
`public.nostos_push_targets(p_scope)`, `security definer`, granted to
`service_role` only, with a doctor check for its absence.

**3. `SqliteWasmStorage` never persisted the direct-mode horizon.**
`Storage::horizon` has a default of `Ok(None)`, which means "fresh database",
so the browser client re-pulled the entire retained change log on every launch
— silently, forever. Found the first time the conformance suite ran on OPFS,
which is exactly why the third leg exists: rusqlite overrode it, in-memory
overrode it, and a property proved on those two is not proved on this one.

**4. A write is not necessarily visible to the very next pull.** The horizon is
`pg_snapshot_xmin`, so a just-committed row stays hidden while ANY older
transaction is still open — and on a Supabase stack `pg_net`'s own queue worker
opens one on a timer. This is the design working, not a bug, but it is a
contract the client has to honour: pull again. The harness polls; `nostos doctor`
reports the same thing as "horizon lag".

## The way back from a 410

Making retention a hard 410 left a hole the plan had marked `ponytail:` — the
device is told to re-snapshot and has nothing to call. `nostos_pull` refusing is
correct; a refusal the device cannot act on is a device bricked by a long
holiday. So the generator now also emits:

```sql
create or replace function public.nostos_snapshot()
returns jsonb   -- one value, for the same reason as the pull
language sql stable security invoker as $$
  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon)
  select h.horizon, null::text, null::text, null::jsonb from h
  union all select h.horizon, 'tasks'::text, null::text, null::jsonb from h
  union all select h.horizon, 'tasks'::text, r.id::text, to_jsonb(r)
            from public.tasks r, h
  -- ...one pair of branches per synced table
$$;
```

Three things are load-bearing, and each has a test:

- **One statement.** The horizon and every table's rows come from a single
  snapshot, so a re-snapshot is as cross-table consistent as a pull. Two
  statements would rebuild the device from two different points in time.
- **`security invoker`.** The same RLS that decides what a pull returns decides
  what the snapshot contains, which is what makes them interchangeable.
- **A header row per table** (`pk` null). A snapshot carries present rows only,
  so without it an empty table is indistinguishable from a table the snapshot
  forgot — and the device would keep rows the server no longer has.

`PullCursor::apply_snapshot` applies it through `ApplyEngine`'s existing
snapshot-window machinery (ADR-0014/ADR-0025), so a row deleted server-side
while the device was away is reaped at the end of its table rather than
lingering forever, and the outbox's pending-local writes are exempt from that
reap. `PostgrestSource::fetch_snapshot` is the transport. The conformance suite
gained a fifth case for it — `a_snapshot_reaps_rows_deleted_while_away` — so it
holds on all three platforms, OPFS included.

Two things remain notes rather than checks, because no assertion can settle
them:

- **"Allow public access" is a dashboard switch.** The policy on
  `realtime.messages` governs PRIVATE channels only. With public access left
  on — which is the default, and what a local stack ships with — the wrong
  tenant joins the same topic by simply not asking for a private one and no
  policy is ever consulted. Verified: the refused tenant is refused with
  `private: true` and admitted with `private: false`.
- **FCM delivery itself is unexercised.** Everything up to the
  `messages:send` call is proven; the call needs a service account.


## Sources (fetched 2026-09-22)

- PostgreSQL 18, [system information functions §9.27.8–9.27.9](https://www.postgresql.org/docs/current/functions-info.html)
  — `pg_current_xact_id`, `xid8` non-wrapping, `pg_current_snapshot`,
  `pg_snapshot_xmin`, "which transactions were committed between two snapshots",
  and the `track_commit_timestamp` caveats.
- PostgreSQL 18, [replication settings](https://www.postgresql.org/docs/current/runtime-config-replication.html)
  — `track_commit_timestamp`.
- [Transactional outbox pattern](https://microservices.io/patterns/data/transactional-outbox.html)
  — atomic data change + notification without 2PC.
- WatermelonDB, [sync backend](https://watermelondb.dev/docs/Sync/Backend)
  — consistent-view requirement; the clock-skew warning; the cross-collection
  sequence alternative.
- RxDB, [replication protocol](https://rxdb.info/replication.html) — RESYNC on
  every reconnect; deterministic ordering.
- Supabase, [custom schemas](https://supabase.com/docs/guides/api/using-custom-schemas) ·
  [Realtime authorization](https://supabase.com/docs/guides/realtime/authorization) ·
  [Broadcast](https://supabase.com/docs/guides/realtime/broadcast)
- Confluent, [JDBC source connector](https://docs.confluent.io/kafka-connectors/jdbc/current/source-connector/overview.html)
  — why timestamp-only incremental queries miss rows.
