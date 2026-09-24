# nostos-infra — adapters

Every concrete implementation of an application port, and the only crate
where `tokio` / `axum` / `postgres` code lives:

- replicators: `PgReplicator` (feature `pg`, ADR-0009), `FakeReplicator`,
  `MirrorReplicator` (ADR-0042)
- session store + sinks: `InMemorySessionStore`, `TokioEventSink`
  (ADR-0040, ADR-0045)
- `/sync` transport: WebSocket (`transport::sync_handler`) and iroh QUIC
  (feature `iroh`, ADR-0041); the JSON wire codec (`wire`)
- auth: `SupabaseJwtAuth` (HS256 + JWKS), `StaticBearerAuth`, `AllowAnonymous`
- write-back: `PgWriteBack` (ADR-0013, ADR-0018); op-log, snapshot and schema
  sources
- push: `PushRouter` with APNs / FCM / Web Push rails (feature `webpush`,
  default on), `RemoteNotifier` delegation to `nostos-pushd`

Depends on `nostos-application` and `nostos-domain`.

```sh
cargo test -p nostos-infra
make pg-e2e   # real-Postgres e2e (docker); tests self-skip without NOSTOS_E2E_PG=1
```

See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) §2.3.
