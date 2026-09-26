# nostos-client — the Nostos Rust SDK

The native Rust client for Nostos: connect to a nostos-server `/sync` endpoint,
apply replicated frames to a durable on-device SQLite store, checkpoint the LSN,
and reconnect with `resume_lsn` on drop. This is the most mature Nostos client —
`#![forbid(unsafe_code)]`, fully tested, workspace `make ci`-gated, and proven
in live-Supabase e2e (`tests/e2e_pg_sync.rs`).

## Public API

`SyncClient<S>` — the tokio orchestrator:

- `SyncClient::new(url, storage, config) -> Self`
- `run_with_reconnect() -> Result<SessionOutcome, ClientError>` — the main loop
  (subscribe with the durable `resume_lsn`, apply, `Ack` each commit, reconnect
  with backoff). `run_once()` for a single session.
- `write(PendingWrite) -> Result<u64, ClientError>` — enqueue a durable write
  (upsert / delete / patch — ADR-0013).
- `checkpoint() -> Result<Lsn>` — flush the apply LSN.
- `subscribe_changes() -> broadcast::Receiver<ApplyOutcome>` — change-tick feed.
- `with_storage(f) -> Result<R, ClientError>` — run a closure on the concrete
  `SqliteStorage` (e.g. `query(sql)` — P1 read surface).

`SqliteStorage` — real `rusqlite` persistence: opaque row bytes per
`(table, pk)` + a `nostos_meta` checkpoint, applied atomically.

`SyncClientConfig` — incl. `dead_letter_max_attempts` (P2 outbox DLQ).

`AppwriteDirectClient` — SQLite apply/outbox with Appwrite Auth and the Atlet
Function journal. `new` calls Appwrite directly; `new_server` sends the same
protocol through a hosted Nostos gateway. Both retain offline writes and clear
another principal's cache on sign-in. The [Atlet Rust runner](../../apps/atlet/README.md)
opens direct and server clients against one hosted journal to prove convergence.

## Run it

```sh
cargo run -p nostos-client --example reactive_scroll   # end-to-end native demo
cargo test -p nostos-client                             # full suite
```

See `examples/reactive_scroll.rs` for a runnable `SqliteStorage` + `SyncClient`
setup, and `src/lib.rs` for the crate-level docs.

## Where this fits

`nostos-client` is the **native** client (tokio + `rusqlite` — not WASM-portable).
Every native SDK wraps this crate's `SyncClient<SqliteStorage>`; the web SDK
binds the WASM-clean `nostos-core` apply engine instead:

| Platform | SDK | Bridge |
|---|---|---|
| Rust (this crate) | `nostos-client` | native |
| Flutter | `sdk/nostos_flutter` | flutter_rust_bridge |
| Swift / Kotlin / .NET | `sdk/nostos_swift`, `sdk/nostos_kotlin`, `sdk/nostos_dotnet` | UniFFI |
| React Native | `sdk/nostos_react_native` | TurboModule over the Swift/Kotlin UniFFI bindings (ADR-0020) |
| Tauri | `sdk/nostos_tauri` | Tauri 2 plugin |
| Node | `sdk/nostos_node` | napi-rs |
| Web/WASM, Capacitor | `sdk/nostos_web` over `crates/nostos-ffi-wasm`; `sdk/nostos_capacitor` runs the web SDK in the webview | wasm-bindgen |

Per-SDK status lives in each SDK's README.

`nostos-core` (the apply engine + `Storage` / `Outbox` traits) is the shared seam;
adding a platform SDK is a thin FFI bridge over it (ADR-0015).

## Status

Shipped + verified. Not yet on crates.io — consume via git/path dep.
License: Apache-2.0.
