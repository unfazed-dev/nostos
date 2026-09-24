# nostos-server — the sync server

The composition root: reads config (env / CLI flags), builds the adapters from
`nostos-infra`, injects them into the `nostos-application` use-cases, verifies
the license (`nostos-license`) and serves `/sync` over axum.

Defaults: `NOSTOS_REPLICATOR=fake` (synthetic events, no Postgres),
`NOSTOS_SYNC_AUTH=none`, `NOSTOS_BIND=0.0.0.0:8800`. Anonymous auth on an
off-host bind refuses to start — bind `127.0.0.1`, pick a real auth mode, or
set `NOSTOS_INSECURE_ANONYMOUS=1` (see [`SECURITY.md`](../../SECURITY.md)).

```sh
make run         # release build of nostos-server, env from your shell
make dev-stack   # docker Postgres + NOSTOS_REPLICATOR=pg
cargo test -p nostos-server
```

Features: `pg` (real replicator), `iroh` (QUIC transport). Env reference:
[`docs/api/README.md`](../../docs/api/README.md); operations:
[`docs/OPERATING.md`](../../docs/OPERATING.md).
