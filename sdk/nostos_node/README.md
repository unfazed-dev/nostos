# @nostos-sync/node

napi-rs bindings exposing `nostos-client`'s `SyncClient<SqliteStorage>` to Node.js.

**Status: v0.1 alpha, not published to npm.** The live round-trip is proven
(`make sdk-e2e node`), but there is no `npm install @nostos-sync/node` yet — no
prebuilt binaries are published, so consumers must build the addon from source.
See [A11](../../docs/plans/nostos-completion-assessment-2026-07-29.md).

## What you get

A single `NostosClient` class. Unlike the UniFFI SDKs, these methods are **genuinely async** —
napi polls them on its own `tokio_rt` worker, so they return real Promises and
never block the event loop.

| method | signature | notes |
|---|---|---|
| `new NostosClient(url, token, dbPath)` | sync | opens SQLite; **no** network I/O |
| `connect()` | `Promise<void>` | opens the WebSocket |
| `subscribe(table, whereSql?)` | `Promise<void>` | starts the live replication run loop |
| `write(table, op, pk, payloadJson?)` | `Promise<number>` | `op` ∈ `upsert` / `delete` / `patch`; returns the outbox id |
| `query(sql)` | `Promise<string>` | JSON array of rows from on-device SQLite |
| `registerPushToken(platform, token)` | `Promise<void>` | `platform` ∈ `fcm` / `apns` / `webpush`; `POST /push-tokens`, same JWT as the sync connection (ADR-0037) |
| `deregisterPushToken(token)` | `Promise<void>` | `DELETE /push-tokens/{token}`; `signOut` calls it for session-registered tokens |
| `close()` | `Promise<void>` | aborts the run loop, drops the session |
| `url` | getter | the configured endpoint |

```js
const { NostosClient } = require("./nostos_node.node");

const nostos = new NostosClient("ws://127.0.0.1:8080/sync", null, "./cairn.db");
await nostos.connect();
await nostos.subscribe("tasks");                    // optional: "status = 'open'"

await nostos.write("tasks", "upsert", "t1", JSON.stringify({ title: "Walk dog" }));
const rows = JSON.parse(await nostos.query("SELECT * FROM tasks"));
await nostos.close();
```

`subscribe` accepts an optional SQL `WHERE` predicate as its second argument —
the server-side row filter. Calling `subscribe` again **replaces** the prior
subscription (its `Drop` aborts the previous run loop).

`write` resolves when the write is durable in the local outbox, **not** when the
server acks it — see
[ADR-0027](../../docs/adr/0027-write-outcome-visibility-in-the-client-sdk.md).

### Push tokens (ADR-0037)

Node has no OS push, so token registration exists for **symmetry** with the
other SDKs (plan task 4.2) — e.g. a host app routing a Web Push subscription
through the same registry:

```js
await nostos.registerPushToken("webpush", subscription.endpoint);
await nostos.deregisterPushToken(subscription.endpoint);
```

Both hit the pinned REST contract (`POST /push-tokens` /
`DELETE /push-tokens/{token}`, `204` on success) with the **same JWT** the
sync connection uses (the handle's cached token — the one `connect()` /
`subscribe()` build the client from). The server stamps tenant/account; the
SDK never attests them. `signOut()` deregisters session-registered tokens
best-effort; a non-`204` rejects with the status + body in the error reason.

## Build

```bash
cd sdk/nostos_node
cargo build --release          # produces the .node addon via napi-derive
# or, for a platform-tagged artifact:
npx napi build --platform --release
```

The addon links `rusqlite` (bundled SQLite), so a C toolchain is required.

## Verify

```bash
make sdk-e2e node          # from the repo root
```

Spawns the shared Rust spine, then runs `smoke_live.cjs` against it — gated on a
real PUSH + ECHO round-trip.

## Ceiling (ponytail)

- **Feasibility scaffold, not a polished SDK.** No `watch`/reactive surface: you
  poll with `query`. A row-tick `ThreadsafeFunction` callback is the upgrade
  point in `src/lib.rs`.
- **No prebuilt binaries.** `napi build --platform` per (OS, arch) plus an npm
  optional-dependency matrix is what a published package needs — A11.
- **`query` returns a JSON string**, not rows — one primitive wide across every
  Nostos FFI surface, deliberately.
- **`publish = false`** in `Cargo.toml`: this crate ships via npm, never
  crates.io.
