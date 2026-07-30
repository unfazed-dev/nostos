# Swift / iOS — UniFFI bindings

Extracted from `sdk/nostos_swift/` on 2026-07-30. Index: [`README.md`](README.md).

Same six members as [`kotlin.md`](kotlin.md) — one Rust surface, two bindings. Swift keeps
lowerCamelCase method names (verified against the `ios-test` app that actually runs).

## Build

```bash
cargo build -p nostos_swift
cargo install uniffi-bindgen-cli --version 0.28    # once
uniffi-bindgen generate --library target/debug/libnostos_swift.dylib \
  --language swift --out-dir sdk/nostos_swift/swift-sources
```

That emits `nostos_swift.swift`, `nostos_swiftFFI.h`, and `nostos_swiftFFI.modulemap`. The repo also
carries an `xcframework/` layout for device + simulator slices. Add the generated `.swift` to your
target and the framework to *Frameworks, Libraries, and Embedded Content*.

## `NostosClient`

| Member | Swift signature | Notes |
|---|---|---|
| init | `NostosClient(url: String, token: String?, dbPath: String)` | `token: nil` for `NOSTOS_SYNC_AUTH=none` |
| `connect` | `func connect() throws` | opens local SQLite + builds the client. **No network.** |
| `subscribe` | `func subscribe(table: String) throws` | **opens the socket**, starts the run loop |
| `write` | `func write(table: String, op: String, pk: String, payloadJson: String?) throws -> UInt64` | durable sequence number |
| `query` | `func query(sql: String) throws -> String` | JSON array **string** — decode with `JSONSerialization` |
| `checkpoint` | `func checkpoint() throws -> UInt64` | durable LSN |

Synchronous and blocking (the Rust side owns a tokio runtime). Keep them off the main thread.

Errors are thrown as the single-variant `NostosError.Message(description:)`.

```swift
let path = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    .appendingPathComponent("cairn.db").path
let client = try NostosClient(url: "ws://127.0.0.1:8800/sync", token: nil, dbPath: path)
try client.connect()
try client.subscribe(table: "tasks")
_ = try client.write(table: "tasks", op: "upsert", pk: "1", payloadJson: #"{"title":"buy milk"}"#)
let rows = try JSONSerialization.jsonObject(
    with: Data(try client.query(sql: "SELECT * FROM tasks").utf8))
```

## Ceilings

- **One table per client** in v1.
- `query` returns JSON text, not rows.
- No reactive stream — poll after writes.
- The simulator reaches a host-loopback server directly; a physical device needs your machine's
  LAN address.

## Proven by

`sdk-e2e` `swift` slice — `xcodebuild` against a booted iPhone simulator, real PUSH + ECHO through
the public API. The harness filters simulator discovery to iPhone/iPad and no longer hardcodes a
UDID or absolute checkout path.
