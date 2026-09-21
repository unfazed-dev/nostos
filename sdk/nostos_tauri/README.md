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
    .plugin(tauri_plugin_cairn::init())
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
```

## Invoke from JS

Sixteen commands, namespaced `plugin:cairn|<command>`:

| command | args | returns |
|---|---|---|
| `connect` | `{ url?, token?, dbPath? }` | `void` — opens SQLite, builds the client, **no network I/O**. All args optional — falls back to the `plugins.cairn` config block |
| `subscribe` | `{ table }` | `void` — starts the live replication run loop |
| `write` | `{ table, op, pk, payloadJson }` | `number` — the outbox id |
| `query` | `{ sql }` | `string` — JSON array of rows |
| `checkpoint` | — | `number` — the durable LSN |
| `watch` | `{ table, onEvent: Channel }` | `void` — reactive full-snapshot push (ADR-0024) |
| `set_token` | `{ token }` | `void` — swap the live credential |
| `sign_out` | — | `void` — ADR-0029 wipe + push deregistration |
| `register_push_token` | `{ platform, token }` | `void` — `POST /push-tokens` (ADR-0037 §3) |
| `deregister_push_token` | `{ token }` | `void` — `DELETE /push-tokens/{token}` |
| `or_set_add` | `{ table, pk, element }` | `number` — ADR-0030 OR-set add (add-wins); table must be in `orSetTables` |
| `or_set_remove` | `{ table, pk, element }` | `number` — OR-set tombstone; a later re-add wins |
| `counter_increment` | `{ table, pk, delta }` | `number` — ADR-0030 PN-Counter increment; table in `counterTables` |
| `counter_decrement` | `{ table, pk, delta }` | `number` — PN-Counter decrement |
| `dead_letters` | — | `{ pending, deadLettered, lastError }` — the ADR-0027 outbox status |
| `connection_state` | — | `boolean` — true once the session has proven a subscription |

**Typed JS/TS bindings live in [`guest-js/`](guest-js/)** — the
`@nostos-sync/tauri` package (ESM + `.d.ts` + README) wrapping every command
with the unified-verb sugar tier (`upsert`/`patch`/`deleteRow`/…). No
build step: plain `.js` + hand-written `.d.ts`, consumed as a path
dependency.

## Config (tauri.conf.json)

The plugin reads a `plugins.cairn` block (see
`example.tauri.conf.json`):

```jsonc
{
  "plugins": {
    "nostos": {
      "syncUrl": "ws://127.0.0.1:8080/sync",
      "token": null,
      "tables": ["tasks", "notes"],
      "dbPath": "cairn.db"
    }
  }
}
```

Every field is optional (absent block == all-defaults); a populated block
lets `connect()` run argless. Precedence: **per-call args > config > floor**
(`"tasks"` / `"cairn.db"`). `deny_unknown_fields` turns a typo'd key
into a loud plugin-init error.

## Push tokens (ADR-0037 §3)

`register_push_token` POSTs `{"platform":"fcm|apns|webpush","token":…}`
to the same origin the sync WS targets (derived from the URL), with the
sync JWT as Bearer — the byte-identical pinned contract the Flutter
(`nostos_database.dart`) and Node SDKs use. On iOS/Android the token comes
from the Tauri mobile shell's APNs/FCM hooks. Desktop has no OS rail: an
online session already receives everything over WS, and doorbells only
target offline devices, so desktop apps usually do not register.
`sign_out` deregisters session-registered tokens best-effort (pre-clear
JWT) — a leaked registration would push the previous principal's data to
the next user.

**Argument names are camelCase, not the Rust snake_case.** `db_path` → `dbPath`,
`payload_json` → `payloadJson`. This is not a guess: `#[tauri::command]` defaults
to `argument_case: ArgumentCase::Camel` and converts each key with
`to_lower_camel_case()` (`tauri-macros-2.6.3/src/command/wrapper.rs:51,507`).
Adding `#[tauri::command(rename_all = "snake_case")]` would flip it. **No JS
caller exists in this repo** — the slice is `cargo test` — so nothing here would
catch a wrong key; that citation is the verification.

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

The plugin's `default` permission set grants all ten commands unconditionally.
Add it to your capability file (see `guest-js/example.capability.json`):

```json
{ "permissions": ["cairn:default"] }
```

A shipped plugin would offer scoped per-table permissions; this scaffold does
not. Registering a command requires **three** edits in lockstep —
`generate_handler!`, the `build.rs` command list (which autogenerates
`allow-<cmd>`), and `permissions/default.toml` — or the ACL rejects the call at
runtime while everything compiles.

## Runtime shape

The ten `#[tauri::command]` handlers are thin wrappers over `impl NostosState`
async methods. `NostosState` **also** owns its own `tokio::runtime::Runtime` — the
home of the long-lived `subscribe()` run loop — so live replication continues
independently of command-handler scheduling.

Rows are observed reactively via `watch` (full snapshot per change tick,
ADR-0024) or by polling `query`. `write` resolves when the write is
durable in the local outbox, **not** when the server acks it; see
[ADR-0027](../../docs/adr/0027-write-outcome-visibility-in-the-client-sdk.md).

## Verify

```bash
make sdk-e2e tauri          # from the repo root
```

**Read this before trusting that result.** The lib slice is `cargo test` (15
tests: offline round-trip, sign-out wipe, reactive watch, live spine E2E, six
push-token contract pins, two config-fallback tests) — it proves the same
`SyncClient<SqliteStorage>` integrates, and it calls `NostosState` methods
directly. It does **not** invoke through Tauri's IPC/ACL layer.

The second rail closes the two gaps that matter:

```bash
cargo test --test conformance   # from sdk/nostos_tauri — also in make sdk-e2e tauri
```

- **Conformance (Track A4)** — `tests/conformance.rs` ports atlet's frozen
  `SyncAdapter` contract v1.1 (`apps/atlet/spec/adapter.md`) to the Tauri
  surface: items 1–3 prove write round-trip, server push visibility, and
  25-write offline queue-drain against a SECOND observer client (marks
  derive only from the normal read path — the fairness rule); item 4 proves
  the sign-out disk wipe (the cold-resync half needs server history the
  spine lacks — that rides the A5 real-server gate); item 5 pins that the
  JS surface imports nothing but `@tauri-apps/api` and covers every command.
  Unlike the flutter pilot, this needs NO operator-provisioned environment —
  the spine is the live backend.
- **Rail integrity** — the ACL lockstep test fails at edit time when
  `build.rs`, `generate_handler!`, `permissions/default.toml`, and the
  autogenerated command files disagree (the exact class that let `subscribe`
  ship unreachable from JS while `cargo test` stayed green).

Both rails run in CI (the `sdk-e2e` job includes the `tauri` slice since
2026-08-27, webkit dev deps installed).

Third rail (2026-09-21): **`fixture/`** — a real Tauri 2 app (`cargo run`
opens a two-table window driven over `window.__TAURI__`), whose
`tests/ipc.rs` submits the frontend's exact `InvokeRequest`s through
`tauri::test::get_ipc_response` — ACL (`capabilities/`), `plugins.cairn`
config, camelCase args — against a live spine. Proves the JS command boundary
on the multi-table shape with no webview. Click-level WebDriver (WebdriverIO
`@wdio/tauri-service`, embedded driver so macOS works) is the upgrade path.

## Ceiling (ponytail)

- **Table set is fixed at `connect`** — `plugins.cairn.tables` (first =
  primary, rest = `extra_tables`, ADR-0022, server cap 32; `table` is the
  one-entry shorthand). Table-taking commands refuse tables outside the set.
  No per-table `where_sql`/`resume_lsn` yet; `subscribe` is one run loop per
  session, `watch` is one pump per table.
- **No published crate / npm package.** A11; `@nostos-sync/tauri` is consumed as a
  path dependency.
- **Push REST has no retry.** A failed POST/DELETE surfaces once (the server
  prunes stale rows rail-side); a retry policy for transient APNs/FCM 5xx is
  the Track B2 upstream item.
- **Registered push tokens are in-memory only.** A process restart forgets
  them (server-side rail pruning covers the stale case) — the
  `registered_push_tokens` ponytail in `src/lib.rs`.
