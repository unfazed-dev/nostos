# @nostos-sync/tauri

Typed JS/TS guest bindings for the [`nostos_tauri`](../README.md) Tauri 2
plugin. Thin wrappers over `invoke("plugin:nostos|…")` with two tiers in one
import:

- **Raw tier** (`nostos.*`) — the exact Rust command surface:
  `connect / subscribe / write / query / checkpoint / watch / setToken /
  signOut / registerPushToken / deregisterPushToken`.
- **Sugar tier** — the unified-verb naming from the nostos DX audit:
  `upsert / patch / deleteRow / writeBatch / watchRows / fetchAll` (object
  payloads, parsed rows).

## Install

The package is not published to npm — consume it as a path dependency from
the nostos repo (arxa pins a tagged nostos checkout):

```jsonc
// package.json
"dependencies": {
  "@nostos-sync/tauri": "file:../nostos/sdk/nostos_tauri/guest-js"
}
```

Requires `@tauri-apps/api` ^2 (peer dependency) and the Rust plugin
registered:

```rust
tauri::Builder::default().plugin(tauri_plugin_nostos::init())
```

plus the capability grant (see `example.capability.json`):

```json
{ "permissions": ["nostos:default"] }
```

## Config (tauri.conf.json)

```jsonc
{
  "plugins": {
    "nostos": {
      "syncUrl": "ws://127.0.0.1:8080/sync",
      "token": null,
      "tables": ["tasks", "notes"],
      "dbPath": "nostos.db"
    }
  }
}
```

All fields optional — with the block populated, `connect()` takes no args.
Per-call args override config; config overrides the floor (`"tasks"` /
`"nostos.db"`). A typo'd key fails plugin init loudly.

## Usage

```js
import { nostos, upsert, watchRows, fetchAll } from "@nostos-sync/tauri";

// connect() does NO network I/O — subscribe()/watch() drives replication.
await nostos.connect();                       // config-supplied defaults
await nostos.subscribe("tasks");

const id = await upsert("tasks", "t1", { title: "Walk dog" });
const stop = watchRows("tasks", (rows) => render(rows));
const all = await fetchAll("SELECT pk, payload FROM nostos_data WHERE table_name = 'tasks'");

// Push registration (ADR-0037 §3) — mobile shells pass the native token:
await nostos.registerPushToken("fcm", fcmToken);
// signOut deregisters session tokens automatically.
```

## Semantics pinned in the Rust crate

- `write` resolves on LOCAL durability (ADR-0013), not server ack.
- `watch` pushes a full snapshot per change tick (ADR-0024) — not a poll.
- `signOut` wipes local state and deregisters push tokens (ADR-0029 +
  ADR-0037 §3).
- Push registration rides `POST /push-tokens` with the sync credential —
  the same pinned REST contract as the Flutter and Node SDKs.

## License

Apache-2.0.
