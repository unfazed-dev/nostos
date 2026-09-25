# Kotlin / Android — UniFFI bindings

Extracted from `sdk/nostos_kotlin/src/lib.rs` on 2026-07-30. Index: [`README.md`](README.md).

This crate is the **canonical UniFFI surface**: Swift ([`swift.md`](swift.md)) and .NET
([`dotnet.md`](dotnet.md)) expose the same six members with their own naming, and React Native
([`react-native.md`](react-native.md)) wraps this exact `.so`.

## Build

```bash
cargo build -p nostos_kotlin                       # produces libnostos_kotlin.{so,dylib}
cargo install uniffi-bindgen-cli --version 0.28    # once
uniffi-bindgen generate --library target/debug/libnostos_kotlin.dylib \
  --language kotlin --out-dir <your-src-dir>
```

There is **no `uniffi-bindgen` bin target in this crate** — the standalone CLI above is the real
path. Ship the `.so` per ABI under `jniLibs/` and the generated `.kt` on your source path.

## `NostosClient`

Constructor at `src/lib.rs:162`, methods at `:188`, `:237`, `:283`, `:340`, `:368`.

| Member | Kotlin signature | Notes |
|---|---|---|
| constructor | `NostosClient(url: String, token: String?, dbPath: String)` | pure handle; no I/O, no runtime work beyond building the tokio runtime |
| `connect` | `fun connect()` | opens local SQLite + builds the `SyncClient`. **No network.** |
| `subscribe` | `fun subscribe(table: String)` | **this is what opens the socket** and spawns the run loop |
| `write` | `fun write(table: String, op: String, pk: String, payloadJson: String?): ULong` | returns the durable sequence number |
| `query` | `fun query(sql: String): String` | a **JSON array string** — UniFFI cannot return `Vec<HashMap>`. Decode it yourself |
| `checkpoint` | `fun checkpoint(): ULong` | the durable LSN resumed from on reconnect |

**All six are synchronous from Kotlin's view** — the Rust side owns a tokio runtime and blocks on
it (`rt.block_on`), a deliberate choice over UniFFI async. Call them off the main thread.

`op` must be `"upsert"`, `"delete"`, or `"patch"`; anything else raises with the expected values
named. Errors surface as `NostosException.Message` carrying a `description` string — the binding
has exactly one error variant (`src/lib.rs:97`).

## Lifecycle

```kotlin
val client = NostosClient("ws://10.0.2.2:8800/sync", null, "${filesDir}/nostos.db")
client.connect()                       // local store only
client.subscribe("tasks")              // ← socket opens here
client.write("tasks", "upsert", "1", """{"title":"buy milk"}""")
val rows = JSONArray(client.query("SELECT * FROM tasks"))
```

From an Android emulator, the host is **`10.0.2.2`**, not `127.0.0.1`.

A second `connect()` replaces the active session: dropping it releases the client **and aborts the
background run loop**, so a superseded session's socket does not leak.

## Ceilings

- **One table per client** in v1. For a second table, construct a second client.
- `query` returns JSON text, not rows.
- No reactive stream — poll `query` after writes, or drive your own ticker. Flutter is the only
  SDK with a reactive layer.

## Proven by

`sdk-e2e` `kotlin` slice — an instrumented Android test on `nostos_api34` doing a real PUSH + ECHO
round-trip against the Rust spine. Needs the AVD booted on **port 5556** (5554 deadlocks the
harness's own boot).
