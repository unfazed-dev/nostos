# nostos-cli — the `nostos` CLI

| command | does |
|---|---|
| `nostos init` | connect to Postgres, create/update the publication, write `nostos.toml` + `.env` |
| `nostos dev` | run `nostos-server` locally from `nostos.toml` + `.env` |
| `nostos doctor` | connectivity, replication health, JWKS reachability checks |
| `nostos deploy` | generate a self-host deploy config (fly/railway) |
| `nostos link` / `pull` / `gen` | app side: scaffold `.nostos/`, fetch `GET /schema`, generate per-SDK source (ADR-0023) |
| `nostos rules init\|edit\|check` | generate/edit/validate `nostos_rules.toml` (ADR-0031) |
| `nostos push init\|check` | configure/validate push credentials (ADR-0037, ADR-0038) |

```sh
cargo run -p nostos-cli -- --help
cargo test -p nostos-cli
```

Walkthrough: [`docs/QUICKSTART.md`](../../docs/QUICKSTART.md).
