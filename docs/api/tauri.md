# Tauri — `nostos-tauri`

Extracted from `sdk/nostos_tauri/src/lib.rs`, `build.rs`, and `permissions/` on 2026-07-30.
Index: [`README.md`](README.md).

A Tauri 2 plugin. The Rust side owns a `nostos-client` (real SQLite, real socket); your frontend
reaches it through `invoke`.

## Install

```rust
// src-tauri/src/lib.rs
tauri::Builder::default()
    .plugin(nostos_tauri::init())
```

## Commands

Five, namespaced `plugin:cairn|<command>`:

| Command | Arguments | Returns |
|---|---|---|
| `connect` | `{ url, token, dbPath }` | `()` — opens local SQLite + builds the client. **No network.** |
| `subscribe` | `{ table }` | `()` — **this is what starts replication** |
| `write` | `{ table, op, pk, payloadJson }` | `u64` — durable sequence number |
| `query` | `{ sql }` | JSON array **string** — `JSON.parse` it |
| `checkpoint` | — | `u64` |

```js
import { invoke } from "@tauri-apps/api/core";

await invoke("plugin:cairn|connect", {
  url: "ws://127.0.0.1:8800/sync", token: null, dbPath: "cairn.db",
});
await invoke("plugin:cairn|subscribe", { table: "tasks" });
await invoke("plugin:cairn|write", {
  table: "tasks", op: "upsert", pk: "1", payloadJson: JSON.stringify({ title: "buy milk" }),
});
const rows = JSON.parse(await invoke("plugin:cairn|query", { sql: "SELECT * FROM tasks" }));
const lsn = await invoke("plugin:cairn|checkpoint");
```

### Argument names are camelCase, and it matters

`#[tauri::command]` defaults to `argument_case: ArgumentCase::Camel` and runs
`to_lower_camel_case()` on every parameter. So the Rust `db_path` / `payload_json` are **`dbPath` /
`payloadJson`** from JS. Passing `db_path` fails at deserialization, not at compile time.

## Adding a command takes three edits

If you extend this plugin, a new command needs **all three** or it fails at runtime while
everything still compiles:

1. `generate_handler![…]` in `src/lib.rs`
2. the command list in `build.rs` (which autogenerates the `allow-<cmd>` permission)
3. `permissions/default.toml`, which must actually grant it

Miss #3 and the ACL rejects the call. Miss #1 or #2 and the command is simply unreachable from JS.
`subscribe` was in **neither** #1 nor #2 until 2026-07-30 — so the whole download path was
unreachable from a frontend while `cargo test` stayed green, because the Rust test called
`NostosState::subscribe` directly and never crossed the command boundary. If you add a command, add
a test that goes through `invoke`.

## Ceilings

- **One table per app instance** in v1.
- `query` returns JSON text, not rows.
- No event stream to the frontend — poll `query` after writes, or emit your own Tauri event from
  the Rust side.

## Proven by

`sdk-e2e` `tauri` slice — Rust integration tests driving connect → subscribe → write → query
against the shared spine. Note this exercises the plugin's Rust surface; the ACL/permission path is
what the three-edit rule above protects.
