# Node (native) — `@nostos-sync/node`

Extracted from `sdk/nostos_node/src/lib.rs` + `package.json` on 2026-07-30.
Index: [`README.md`](README.md).

A napi-rs native addon over `nostos-client`: **real SQLite, real WebSocket, genuinely async.**
Distinct from `@nostos-sync/web`'s Node facade, which is an apply engine with no socket — see
[`web.md`](web.md) if you are unsure which you want.

`package.json` sets `"main": "nostos_node.node"` — you `require` the compiled addon directly.

## Build

```bash
cargo build -p nostos_node --release      # emits nostos_node.node
```

## `NostosClient`

Constructor at `src/lib.rs:92`; methods at `:111`, `:120`, `:152`, `:204`, `:252`, `:276`.

| Member | Signature | Notes |
|---|---|---|
| constructor | `new NostosClient(url: string, token: string \| null, dbPath: string)` | pure handle |
| `url` | `get url(): string` | getter |
| `connect` | `connect(): Promise<void>` | opens local SQLite + builds the client. **No network.** |
| `subscribe` | `subscribe(table: string, whereSql?: string \| null): Promise<void>` | **opens the socket**; `whereSql` is the server-compiled predicate |
| `write` | `write(table: string, op: string, pk: string, payloadJson?: string \| null): Promise<number>` | durable sequence number (a JS `number`, from Rust `f64`) |
| `query` | `query(sql: string): Promise<string>` | JSON array **string** — `JSON.parse` it |
| `close` | `close(): Promise<void>` | tears the session down |

These are **real `async` methods** polled on napi's tokio worker — not sync-over-block like the
UniFFI bindings. `await` them normally.

`op` is `"upsert" | "delete" | "patch"`.

```js
const { NostosClient } = require("@nostos-sync/node");

const c = new NostosClient("ws://127.0.0.1:8800/sync", null, "./nostos.db");
await c.connect();                       // local store only
await c.subscribe("tasks");              // ← socket opens here
await c.write("tasks", "upsert", "1", JSON.stringify({ title: "buy milk" }));
const rows = JSON.parse(await c.query("SELECT * FROM tasks"));
await c.close();
```

A second `subscribe()` drops the prior session first, aborting its run loop — no leaked socket.

## Ceilings

- **One table per client** in v1 — construct a second client for a second table.
- `query` returns JSON text, not rows.
- No reactive stream; poll after writes.
- `publish = false` on the crate — consume by path/git for now.

## Note on `unsafe`

`unsafe` is forbidden workspace-wide. `napi-derive`'s `#[napi]` macro is machine-generated FFI
glue and is the documented exception, same category as the flutter_rust_bridge codegen. No
hand-written `unsafe` here.

## Proven by

`sdk-e2e` `node` slice — `smoke_live.cjs` against the shared Rust spine, printing `PUSH_OK` and
`ECHO_OK` for a full round-trip through this public API.
