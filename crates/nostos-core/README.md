# nostos-core — the client apply engine

The platform-agnostic half of every client: `ApplyEngine` consumes `/sync`
frames, applies them to a `Storage`, and advances the durable LSN checkpoint
so a reconnect resumes where it left off. Also the `Outbox` for offline
writes, the pull cursor, attachment state (ADR-0034) and `InMemoryStorage`.

WASM-clean: no tokio, no SQLite, depends on `nostos-domain` only. The native
client (`nostos-client`, rusqlite) and the browser bridge (`nostos-ffi-wasm`)
both run this engine. Feature `conformance` exposes the direct-mode
conformance suite every platform runs.

```sh
cargo test -p nostos-core
```
