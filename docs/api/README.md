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

| SDK | Page | Package | Live sync | Local reads |
|---|---|---|---|---|
| Flutter / Dart | [`flutter.md`](flutter.md) | `nostos_flutter` | ✅ | **SQL** over SQLite views |
| Web (browser) | [`web.md`](web.md) | `@nostos-sync/web` → `pkg-web` | ✅ | in-memory KV (`rowsFor`) |
| Web (Node) | [`web.md`](web.md) | `@nostos-sync/web` → `index.js` | ❌ apply-engine only | in-memory KV |
| Node (native) | [`node.md`](node.md) | `@nostos-sync/node` | ✅ | **SQL** over SQLite |
| React Native | [`react-native.md`](react-native.md) | `@nostos-sync/react-native` | ✅ (Android) | **SQL** over SQLite |
| Capacitor | [`capacitor.md`](capacitor.md) | `@nostos-sync/capacitor` | ✅ | KV via WASM engine |
| Tauri | [`tauri.md`](tauri.md) | `nostos-tauri` | ✅ | **SQL** over SQLite |
| Kotlin / Android | [`kotlin.md`](kotlin.md) | UniFFI `.so` + bindings | ✅ | **SQL** over SQLite |
| Swift / iOS | [`swift.md`](swift.md) | UniFFI `.xcframework` | ✅ | **SQL** over SQLite |
| .NET / MAUI | [`dotnet.md`](dotnet.md) | `Nostos.DotNet` | ✅ | **SQL** over SQLite |

**None of these are published to a registry yet** — all five Rust SDK crates carry
`publish = false`, and `@nostos-sync/capacitor` depends on `@nostos-sync/web` by relative path. Consume them
from a path/git dependency for now. See [`../IDENTITY.md`](../IDENTITY.md).

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
PowerSync's `uploadData` (ADR-0013), and it is why no SDK here has a connector class.

**Write-back is gated server-side and empty by default.** Set `NOSTOS_WRITE_TABLES=tasks,…` or
every write is refused. This trips up everyone once.

## Server

`nostos-server` is the only thing your clients talk to.

| Route | Purpose |
|---|---|
| `GET {NOSTOS_WS_PATH}` (default `/sync`) | the WebSocket. Auth token goes on the query string as `?token=` — browsers cannot set handshake headers |
| `GET /schema` | typed publication schema (ADR-0021). **404 unless `NOSTOS_REPLICATOR=pg`** |
| `GET /healthz` | liveness |

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
in for RLS on sync traffic — read [`../SECURITY-MODEL.md`](../SECURITY-MODEL.md) before a
multi-tenant deploy.

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
