# nostos-tauri

Tauri 2 plugin exposing `nostos-client`'s `SyncClient<SqliteStorage>` to desktop
web apps.

**Status: v0.1 alpha, not published to crates.io.** The Rust integration is
covered by tests (`make sdk-e2e tauri`), but see the honest caveat under
*Verify* — the e2e exercises `NostosState` directly, not the JS command boundary.
See [A11](../../docs/plans/nostos-completion-assessment-2026-07-29.md).

## Register the plugin

```rust
// src-tauri/src/lib.rs
tauri::Builder::default()
    .plugin(nostos_tauri::init())
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
```

## Invoke from JS

Five commands, namespaced `plugin:cairn|<command>`:

| command | args | returns |
|---|---|---|
| `connect` | `{ url, token, dbPath }` | `void` — opens SQLite, builds the client, **no network I/O** |
| `subscribe` | `{ table }` | `void` — starts the live replication run loop |
| `write` | `{ table, op, pk, payloadJson }` | `number` — the outbox id |
| `query` | `{ sql }` | `string` — JSON array of rows |
| `checkpoint` | — | `number` — the durable LSN |

```js
import { invoke } from "@tauri-apps/api/core";

await invoke("plugin:cairn|connect", {
  url: "ws://127.0.0.1:8080/sync",
  token: null,
  dbPath: "cairn.db",
});

// REQUIRED: connect() does no network I/O. Without subscribe() nothing drives
// the run loop, so no server-pushed row ever arrives.
await invoke("plugin:cairn|subscribe", { table: "tasks" });

await invoke("plugin:cairn|write", {
  table: "tasks", op: "upsert", pk: "t1",
  payloadJson: JSON.stringify({ title: "Walk dog" }),
});

const rows = JSON.parse(await invoke("plugin:cairn|query", {
  sql: "SELECT * FROM tasks",
}));
const lsn = await invoke("plugin:cairn|checkpoint");
```

`subscribe` was **added on 2026-07-30**. Before that the plugin registered only
`connect`/`write`/`query`/`checkpoint`, so the entire download path was
unreachable from JS — a frontend could connect and then wait forever. It stayed
invisible because the Rust test calls `NostosState::subscribe` directly and never
crosses the command boundary. If you pinned an earlier commit, that is the bug.

### Permissions

The plugin's `default` permission set grants all five commands unconditionally.
Add it to your capability file:

```json
{ "permissions": ["cairn:default"] }
```

A shipped plugin would offer scoped per-table permissions; this scaffold does
not. Registering a command requires **three** edits in lockstep —
`generate_handler!`, the `build.rs` command list (which autogenerates
`allow-<cmd>`), and `permissions/default.toml` — or the ACL rejects the call at
runtime while everything compiles.

## Runtime shape

The five `#[tauri::command]` handlers are thin wrappers over `impl NostosState`
async methods. `NostosState` **also** owns its own `tokio::runtime::Runtime` — the
home of the long-lived `subscribe()` run loop — so live replication continues
independently of command-handler scheduling.

Received rows are observed by polling `query`. `write` resolves when the write is
durable in the local outbox, **not** when the server acks it; see
[ADR-0027](../../docs/adr/0027-write-outcome-visibility-in-the-client-sdk.md).

## Verify

```bash
make sdk-e2e tauri          # from the repo root
```

**Read this before trusting that result.** The slice is `cargo test` — it proves
the same `SyncClient<SqliteStorage>` integrates, and it calls `NostosState`
methods directly. It does **not** invoke through Tauri's IPC/ACL layer, so it
cannot catch a command missing from `generate_handler!` or `default.toml` (which
is exactly how the `subscribe` gap survived). A WebDriver-driven Tauri app
asserting on `invoke()` is the real coverage, and it is not written.

## Ceiling (ponytail)

- **One table per `NostosState`**, hardcoded to `tasks` in `connect` — `subscribe`
  errors if `table` mismatches. Multi-table is the
  [provider-dashboard plan](../../docs/plans/nostos-provider-dashboard-multitable.md).
- **No row-tick events.** Rows are polled via `query`; a Tauri `Channel` that
  emits on apply is the upgrade point.
- **No published crate.** A11.
