# Nostos API reference

One page per SDK. **Every signature here was extracted from source on 2026-07-30**, and each
page cites the file it came from so you can check it yourself.

> **Why the citations.** On 2026-07-30 both `sdk/nostos_flutter/README.md` and its `USAGE.md`
> documented `NostosDatabase.supabase(supabaseUrl:, supabaseAnonKey:, accessToken:)` — three
> parameters that never existed. They had been wrong for weeks because nothing checks prose:
> `make ci` is Rust-only and `dart analyze` does not compile fenced markdown. These pages are a
> third place the same signatures could rot, so `sdk/nostos_flutter/scripts/check-doc-signatures.py`
> now validates their Dart snippets against the real factories, and it runs in the `flutter`
> `sdk-e2e` slice. If you add a snippet, run `make sdk-e2e flutter` — or at minimum that script.

## Pick your SDK

> **The two WASM-engine rows are live-only for writes.** Web (browser, via `NostosSocket`) and
> Capacitor send a write straight to the socket and never touch the outbox, so a write with the
> socket closed **throws** instead of queueing, and rows do not survive a reload. Every SQLite-backed
> row enqueues durably *before* any network call. See the
> [ADR-0017 addendum](../adr/0017-web-persistence.md).

| SDK | Page | Package | Live sync | Local reads |
|---|---|---|---|---|
| Flutter / Dart | [`flutter.md`](flutter.md) | `nostos_flutter` | ✅ | **SQL** over SQLite views |
| Web (browser) | [`web.md`](web.md) | `@nostos-sync/web` → `pkg-web` | ✅ | in-memory KV (`rowsFor`) |
| Web (Node) | [`web.md`](web.md) | `@nostos-sync/web` → `index.js` | ❌ apply-engine only | in-memory KV |
| Node (native) | [`node.md`](node.md) | `@nostos-sync/node` | ✅ | **SQL** over SQLite |
| React Native | [`react-native.md`](react-native.md) | `@nostos-sync/react-native` | ✅ (Android) | **SQL** over SQLite |
| Capacitor | [`capacitor.md`](capacitor.md) | `@nostos-sync/capacitor` | ✅ | KV via WASM engine |
| Tauri | [`tauri.md`](tauri.md) | `tauri-plugin-cairn` | ✅ | **SQL** over SQLite |
| Kotlin / Android | [`kotlin.md`](kotlin.md) | UniFFI `.so` + bindings | ✅ | **SQL** over SQLite |
| Swift / iOS | [`swift.md`](swift.md) | UniFFI `.xcframework` | ✅ | **SQL** over SQLite |
| .NET / MAUI | [`dotnet.md`](dotnet.md) | `Nostos.DotNet` | ✅ | **SQL** over SQLite |

**None of these are published to a registry yet** — all five Rust SDK crates carry
`publish = false`, and `@nostos-sync/capacitor` depends on `@nostos-sync/web` by relative path. Consume them
from a path/git dependency for now. See [`../IDENTITY.md`](../IDENTITY.md).

## Why the SDKs differ

Two independent reasons, and they deserve different treatment: **design around the first, expect
the second to disappear.** Do not read the matrix above as one taxonomy.

### 1. SQL vs KV reads — the crate graph (design around this)

No SDK chose this. It follows from which crate a binding can reach.

`nostos-core`'s entire dependency list is `nostos-domain`, `serde`, `serde_json`, `thiserror` — **no
tokio, no rusqlite.** It is WASM-clean by construction, and the crate map in `CLAUDE.md` makes that
a rule violations fail review over. `nostos-ffi-wasm` then depends on exactly `nostos-core` +
`wasm-bindgen`.

| Binding | Reaches | `Storage` impl | Reads look like |
|---|---|---|---|
| Flutter, Node, RN, Tauri, Kotlin, Swift, .NET | `nostos-client` | `SqliteStorage` (rusqlite) | SQL over views |
| Browser, Capacitor | `nostos-core` via WASM | `InMemoryStorage` — `BTreeMap<(table,pk),(bytes,lsn)>` + a `BTreeMap` outbox | `rowsFor(table)`, bytes |

The `Storage` trait (`crates/nostos-core/src/storage.rs:52`) is the seam that lets one apply engine
serve both. That is the hexagonal boundary working, not a compromise.

**This is not because SQLite cannot run in WASM.**
[ADR-0017](../adr/0017-web-persistence.md) evaluated three options and **commits to SQLite-WASM
with the `opfs-sahpool` VFS** after launch, explicitly rejecting wa-sqlite and raw OPFS. So the KV
tier is a deferred slice with a chosen destination, not a platform ceiling.

What actually blocks it is the **threading model, not SQL**: `createSyncAccessHandle` is Worker-only
by spec, and `nostos-ffi-wasm` runs on the main thread today. Going durable means spawning a
dedicated Worker, defining a `postMessage` command protocol, marshalling `RowOp`/`PendingWrite`
across it, and **moving the WebSocket transport too** — it cannot call sync storage from the main
thread. Until then the browser's durability story is the `localStorage` checkpoint plus
replay-from-`resume_lsn`, and Safari Private Browsing disallows OPFS, so any durable backend will
still need the in-memory fallback.

ADR-0017 also found **no prior art for nostos's shape**: RxDB, Dexie, ElectricSQL and
Triplit are all TypeScript already running in a Worker. None is a Rust→wasm client with a `Storage`
trait on the main thread.

### 2. Reactive vs poll — where the work stopped (expect this to change)

Not architectural. Nothing prevents a Kotlin or Node equivalent of Flutter's `Collection<T>`:
same core, same trait, same capability. [ADR-0024](../adr/0024-client-reactive-facade-and-query-primitive.md)
built the reactive facade for Flutter because Flutter was the launch target, and the other eight
simply do not have one yet. If you are picking an SDK today, poll after writes; do not architect
around the absence.

## The shape every SDK shares

Five SDKs (Kotlin, Swift, .NET, React Native, Node) are thin bindings over the same Rust
`nostos-client`, so they share one lifecycle. Flutter adds a reactive layer on top; the two
web paths differ most.

```
construct(url, token, dbPath)   →  no I/O, just a handle
  connect()                     →  opens local SQLite + builds the client. STILL NO NETWORK.
    subscribe(table)            →  ← this is what starts the socket and the run loop
      write(...)                →  applies locally at once, queues durably, syncs in background
      query(sql) / watch(...)   →  read your local store
      checkpoint()              →  the durable LSN you resume from
```

**`connect()` does not connect.** It opens the database. `subscribe()` opens the socket. Getting
this backwards is the most common way to sit waiting for rows that were never requested.

### Write ops

`op` is one of exactly three strings, checked at the boundary
(`sdk/nostos_kotlin/src/lib.rs:283`, which rejects anything else by name):

| `op` | Meaning |
|---|---|
| `upsert` | insert-or-replace the whole row |
| `delete` | remove the row (payload is `null`) |
| `patch` | update only the columns present — per-field last-write-wins |

Writes are **collapsed**: you do not implement an upload endpoint. The server's `PgWriteBack`
applies queued mutations to Postgres directly. This is the deliberate difference from
a split `uploadData` model (ADR-0013), and it is why no SDK here has a connector class.

**Write-back is gated server-side and empty by default.** Set `NOSTOS_WRITE_TABLES=tasks,…` or
every write is refused. This trips up everyone once.

## Server

`nostos-server` is the only thing your clients talk to.

| Route | Purpose |
|---|---|
| `GET {NOSTOS_WS_PATH}` (default `/sync`) | the WebSocket. Auth token goes on the query string as `?token=` — browsers cannot set handshake headers |
| `GET /schema` | typed publication schema (ADR-0021). **404 unless `NOSTOS_REPLICATOR=pg`** |
| `GET /healthz` | liveness |
| `POST /push-tokens` / `DELETE /push-tokens/{token}` | push-token registration (ADR-0037). Rails, `NOSTOS_PUSH_TABLES` templates and the experimental Live Activities mode: [`push.md`](push.md) |

Environment variables, from `crates/nostos-server/src/main.rs` +
`crates/nostos-infra/src/`. The ones you will actually set:

| Variable | Notes |
|---|---|
| `NOSTOS_PG_URL` | Postgres connection string |
| `NOSTOS_REPLICATOR` | `pg` for real replication; anything else uses the fake generator **and disables `/schema` + the snapshotter** |
| `NOSTOS_WRITE_TABLES` | write-back allowlist. **Empty = all writes refused** |
| `NOSTOS_SYNC_AUTH` | `none` \| `supabase-jwt` |
| `NOSTOS_TENANT_COLUMN` | the column tenant isolation is enforced on |
| `NOSTOS_BIND`, `NOSTOS_WS_PATH`, `NOSTOS_LOG`, `NOSTOS_CORS_ORIGINS` | transport / logging |
| `NOSTOS_SUPABASE_JWKS_URL`, `NOSTOS_SUPABASE_JWT_SECRET`, `NOSTOS_SUPABASE_URL` | Supabase auth |
| `NOSTOS_PG_PUBLICATION`, `NOSTOS_PG_SLOT`, `NOSTOS_PG_SLOT_WAL_KEEP_SIZE`, `NOSTOS_SLOT_MAX_LAG` | replication slot |
| `NOSTOS_OPLOG_BUFFER`, `NOSTOS_OPLOG_RETENTION_SECS`, `NOSTOS_OPLOG_COMPACT_INTERVAL_SECS` | backfill oplog (ADR-0025) |
| `NOSTOS_SESSION_BUFFER`, `NOSTOS_FAKE_EPS`, `NOSTOS_FAKE_KEYS` | fan-out tuning / fake generator |
| `NOSTOS_TIER`, `NOSTOS_LICENSE`, `NOSTOS_LICENSE_SECRET` | licensing |

Nostos's server holds a privileged Postgres connection: **logical replication and write-back both
bypass Row Level Security by construction.** Nostos's own predicates and tenant enforcement stand
in for RLS on sync traffic. Two docs, and you want both before a multi-tenant deploy — their
titles both read as "Security Model", so it is easy to read one and think you are done:

- [`../SECURITY-MODEL.md`](../SECURITY-MODEL.md) — *conceptual*: why RLS cannot reach sync traffic
  and what Nostos substitutes for it.
- [`../SECURITY.md`](../SECURITY.md) — *operational*: the collapsed-write model, the least-privilege
  `BYPASSRLS` role (**not** superuser), Supabase setup, and the RLS trade-off. This is the one with
  the role setup you actually have to perform.

(A third file, `SECURITY.md` at the repo root, is the vulnerability-reporting policy — unrelated.)

## CLI

From `crates/nostos-cli/src/main.rs:22`:

| Command | Does |
|---|---|
| `nostos init` | scaffold a `.nostos/` project |
| `nostos link` | attach a backend (e.g. Supabase project) |
| `nostos pull` | fetch the live schema into the project |
| `nostos gen` | generate typed client code (`nostos.g.dart`) |
| `nostos dev` | run a local server against your project |
| `nostos doctor` | diagnose a project's setup |
| `nostos deploy` | deploy the server |

`nostos pull && nostos gen` is the loop that keeps generated schema in step with Postgres.

`nostos pull` reads `GET /schema` from the linked server. If that server runs with
`NOSTOS_PROTECT_METADATA=1` the endpoint requires a bearer token: pass `--token <TOKEN>` or set
`NOSTOS_TOKEN` in the environment and the CLI sends `Authorization: Bearer <token>`. The token must
satisfy the server's `NOSTOS_SYNC_AUTH` adapter (the `NOSTOS_SYNC_BEARER_TOKEN` secret for `bearer`, a
user JWT for `supabase-jwt`). It is never written to `.nostos/config.json` (that file is committed)
and never printed. Prefer `NOSTOS_TOKEN` over `--token` on shared machines (`--token` is visible in
`ps`), and note that a token sent to a plain `http://` server travels in cleartext.

## Wire protocol

JSON, deliberately human-debuggable until a measurement says otherwise
(`crates/nostos-infra/src/wire.rs`). You do not write this by hand — it is documented so you can
read a socket dump. `ClientMessage` is `{"type": "subscribe"|"ack"|"write", …}`; `subscribe`
carries `table`, `filters`, `where_sql`, `resume_lsn`, `epoch`.

## Related

- Architecture: [`../ARCHITECTURE.md`](../ARCHITECTURE.md)
- Operations: [`../OPERATING.md`](../OPERATING.md)
- Why local reads are views, not typed tables: [`../adr/0028-client-read-views-over-opaque-payload.md`](../adr/0028-client-read-views-over-opaque-payload.md)
- Decision log: [`../adr/`](../adr/)
