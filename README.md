# Nostos

[![CI](https://github.com/unfazed-dev/nostos/actions/workflows/ci.yml/badge.svg)](https://github.com/unfazed-dev/nostos/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/rust-1.98-orange)

Nostos is an Apache-2.0 local-first sync engine. Its Rust apply engine stores
app data and a durable write queue in on-device SQLite, so reads and writes
continue while the device is offline. When connectivity returns, Nostos
reconciles changes with the selected cloud database.

Nostos has two transport modes. **Direct** is the default: the device syncs
with a hosted backend using a database-side change journal, without an
always-on Nostos server. **Server** uses `nostos-server` and Postgres logical
replication for a self-hosted WebSocket sync service. The app-facing local
database and write API stay the same. See [the architecture](docs/ARCHITECTURE.md)
and [the direct protocol](docs/plans/direct-mode-sync-protocol.md).

## Providers and current status

| Path | Cloud source | Current implementation |
|---|---|---|
| Supabase direct | Postgres change journal and RPC, Realtime wake-ups | Native client and Flutter integration; generated SQL and a real Supabase-stack test |
| Appwrite direct | TablesDB journal and hosted Rust Function | Native client and Atlet Flutter macOS/Chrome cloud tests with an admin and two customers |
| Nostos server | Postgres logical replication and WebSocket | Rust server and SDK transports; [operator runbook](docs/OPERATING.md) |

Direct mode is selected per signed-in session. The Appwrite Flutter web path
uses SQLite-WASM with durable browser storage; its server mode and physical
push checks are in progress. Each provider has its own schema and credentials.
The [Atlet reference app](apps/atlet/README.md) is the shared visual scenario
for validating them. Its full cross-SDK coverage is being built against the
[cloud reference contract](docs/plans/atlet-cross-sdk-cloud-reference-2026-09-26.md).

## See Nostos run

For the fastest local client demonstration, with no Docker or cloud account:

```sh
cargo run -p nostos-client --example reactive_scroll
```

This uses synthetic replication events and exercises the real SQLite apply,
reconnect, and durable checkpoint paths. For a real Postgres source, follow
the [quick start](docs/QUICKSTART.md) or run `make dev-stack`; the Docker
database uses port `5433`. `make pg-e2e` runs the real logical-replication
tests with a disposable test database.

For the hosted Appwrite Atlet demo, see [Appwrite setup](apps/atlet/README.md#appwrite-cloud-setup).
The repeatable visual test is a Rust command:

```sh
cargo run -p atlet-harness --bin appwrite_flutter_smoke -- --device macos --scenario order
cargo run -p atlet-harness --bin appwrite_flutter_web_smoke -- --role admin
```

Both commands use one hosted project with an admin and two customers. They
exercise an offline purchase, cloud fulfilment, and customer isolation. The
Chrome run also checks browser reload while offline and cloud replay from a
second Nostos client. An ignored credentials file is required; the setup guide
explains its format. The scenarios keep delivered orders as demo history and
remove their catalog fixtures.

## Repository map

| Location | Purpose |
|---|---|
| `crates/nostos-domain`, `nostos-application` | Invariants and use-case ports |
| `crates/nostos-infra`, `nostos-server` | Postgres, transport, auth, write-back, and server composition |
| `crates/nostos-core`, `nostos-client`, `nostos-ffi-wasm` | Shared apply engine, native SQLite client, and browser bridge |
| `sdk/` | Flutter, Swift, Kotlin, React Native, .NET, Tauri, Capacitor, Node, and web packages |
| `apps/atlet/` | Visual reference app, hosted Appwrite schema and Function, Rust cloud runners |
| `docs/adr/`, `docs/api/` | Decisions and SDK API reference |

The core has no I/O or async runtime dependency. Native SDKs wrap the Rust
client; browser SDKs use the WASM bridge and durable browser storage when
available. The [architecture guide](docs/ARCHITECTURE.md) explains the crate
boundaries and each transport.

## Develop and verify

Use the current stable Rust toolchain and the Flutter version recorded by the
repo. The main gate is:

```sh
make ci
```

It runs Rust format, Clippy with warnings denied, and the workspace tests.
`scripts/check.sh [area]` runs a CI job locally. Real Postgres tests require
`make pg-e2e`; `scripts/check.sh atlet-cloud` and
`scripts/check.sh atlet-web-cloud` run the hosted multi-user Atlet suites with
the ignored demo credentials. The matching PR checks use scoped repository
secrets; Atlet changes must pass them before merge. See [contributing](CONTRIBUTING.md) and the
[operator runbook](docs/OPERATING.md).

## Performance

The eval-only fan-out harness measured **2,618,601 ops/sec aggregate at 1,000
clients with 0.00% drops** (median of three, 2026-09-02, FakeReplicator on
loopback). The separate full-path real-Postgres-to-client-apply measurement was
**about 34.8k–36.2k rows/sec sustained**, with zero drops at buffer 32768
(2026-08-24). These measure different stages and must not be divided into a
cross-stage ratio. Read the [results](benches/results/RESULTS.md) and
[methodology](docs/BENCHMARK-METHODOLOGY.md) before citing either figure.

## License and security

Nostos is [Apache-2.0](LICENSE) across the server, core, and SDKs. See
[SECURITY.md](SECURITY.md) for vulnerability reporting and the sync security
model. The project is alpha; use the [roadmap](docs/ROADMAP.md) for release
readiness and current work.
