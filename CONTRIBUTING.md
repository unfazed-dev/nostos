# Contributing to Nostos

Pre-1.0; architecture is pinned by ADRs (docs/adr/), code is moving fast.

## Setup
- Rust 1.95.0 (rust-toolchain.toml installs it), Docker (for the Postgres e2e).
- `make setup` then `make ci` must pass before and after your change.

## Rules
- Dependency direction: bootstrap → application → domain ← infrastructure. See ADR-0001.
- `unsafe` is forbidden. Clippy pedantic is on; CI fails on warnings.
- Architectural changes need an ADR (docs/adr/NNNN-slug.md) before code.
- Perf changes need before/after numbers (docs/BENCHMARK-METHODOLOGY.md).
- Commits: single line, conventional prefix (`feat:`/`fix:`/`test:`/`docs:`/`chore:`).
- Tests accompany every non-trivial change; the e2e suite for replication changes:
  `NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg`.

## License
Apache-2.0. By contributing you agree your work is licensed the same way.
New dependencies must be Apache-2.0-compatible (checked by `cargo deny` in CI).
