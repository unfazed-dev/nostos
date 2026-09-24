# nostos-application — use-cases and ports

The driven-side port traits (`src/ports.rs`: `ReplicatorStream`,
`SessionStore`, `EventSink`, `SyncAuth`, `WriteBack`, `OpLogWriter` /
`OpLogSource`, `SnapshotSource`, `SchemaSource`, `TableStatsSource`,
`PushNotifier`) and the use-cases that drive them: `FanOutService` (the hot
fan-out loop) and `SessionManager` (connect/disconnect under the tier device
cap).

Depends on `nostos-domain` only. Adapters live in `nostos-infra`; tests here
use hand-rolled fakes, no network and no Postgres.

```sh
cargo test -p nostos-application
```

See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) §2.2 and §3.
