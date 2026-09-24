# nostos — full name map (proposal, 2026-09-24)

Every place `cairn` appears as an identifier today, and what it becomes under
**nostos** (Greek νόστος, "homecoming": the offline device comes home and
catches up). Availability checked 2026-09-24; ✅ = free, ❌ = taken, ~ = DNS-only.
Nothing here is registered or published yet. Supersedes the `qairn` target in
`qairn-rename-inventory-2026-09-21.md`; that inventory's §2 risk analysis still applies.

## Registries and domains

| slot | name | status |
|---|---|---|
| primary domain | `nostos.run` | ✅ free (RDAP) — `nostos.build`, `nostos.tools`, `nostos.software`, `nostos.codes` also free |
| later / buy | `nostos.app` (for sale, Spaceship; lapses 2026-12-21), `nostos.co` (Afternic) | 💰 |
| taken | `nostos.com` `.dev` `.io` `.sh` `.ai` `.tech` `.network` `.cloud` … | ❌ (`.sync` is not a TLD) |
| GitHub repo | `unfazed-dev/cairn` → `unfazed-dev/nostos` | ✅ (GitHub redirects the old URL) |
| crates.io bare `nostos` | — not used | ❌ benferse v0.1.0 (Apr 2026, 20 dl) |
| crates.io `nostos-*` | see crate table | ✅ all 18 + `tauri-plugin-nostos` |
| npm scope | `@nostos-sync` | ✅ (`@nostos` = empty org owned by someone else; `@nostos-run` also ✅) |
| npm bare `nostos` | — | ❌ 2022 auto-commit tool v0.0.1 |
| pub.dev | `nostos_flutter` (bare `nostos` also ✅) | ✅ |
| NuGet | `Nostos.DotNet` | ✅ |
| Maven groupId | `run.nostos` (verify via DNS TXT on nostos.run) | needs domain |
| CocoaPods | `NostosCapacitor` | ✅ |
| Homebrew | generic tap repo `unfazed-dev/homebrew-tap`, formula `nostos` → `brew install unfazed-dev/tap/nostos`; homebrew-core `nostos` once notable (≥75 stars) → `brew install nostos` | ✅ (tap repo free; homebrew-core has no `nostos`) |
| container image | `ghcr.io/unfazed-dev/nostos` (Docker Hub `nostos` namespace is taken) | ✅ |
| email | `founders@nostos.run` (replaces the unregistered `founders@cairn.dev`) | needs domain |

## Rust crates (workspace)

| today | nostos | binaries |
|---|---|---|
| cairn-domain | nostos-domain | |
| cairn-application | nostos-application | |
| cairn-infra | nostos-infra | |
| cairn-server | nostos-server | `nostos-server` |
| cairn-core | nostos-core | |
| cairn-client | nostos-client | |
| cairn-ffi-wasm | nostos-ffi-wasm | (wasm: `nostos_ffi_wasm_bg.wasm`) |
| cairn-bench | nostos-bench | `nostos-bench`, `nostos-bench-10k`, `nostos-fanout-walk`, `nostos-reconnect-storm`, `nostos-bench-pg-ingest` |
| cairn-cloud | nostos-cloud | `nostos-cloud` |
| cairn-license | nostos-license | |
| cairn-push | nostos-push | `nostos-pushd` |
| cairn-cli | nostos-cli | **`nostos`** |

SDK-side crates (`publish = false`): `nostos-dotnet` (lib `nostos_dotnet`),
`nostos-kotlin` (lib `nostos_kotlin`), `nostos-swift` (lib `nostos_swift`),
`nostos_node`, `nostos_flutter_rust`, `tauri-plugin-nostos`, `nostos-fixture`.

## CLI

```sh
# install (install script is new — needs GitHub release artifacts, none exist yet)
curl -fsSL https://nostos.run/install.sh | sh
brew install unfazed-dev/tap/nostos # generic tap: no doubled name; later `brew install nostos` via homebrew-core
cargo install nostos-cli            # installs the `nostos` binary

nostos init
nostos rules init | edit | check    # writes nostos_rules.toml
nostos dev
nostos doctor
nostos link --visible --deploy
nostos pull | gen | push
nostos deploy
```

`npx nostos` is **not** available (bare npm name taken); an npm CLI would be `npx @nostos-sync/cli`.

## SDK packages and public types

| platform | package / module | main types |
|---|---|---|
| Rust | `nostos-client` | `Nostos` |
| Flutter | `nostos_flutter` — `import 'package:nostos_flutter/nostos_flutter.dart'` | `Nostos`, `NostosDatabase`, `NostosHandle`, `NostosEngine`, `NostosConnectionState`, `NostosWriteInput`, `NostosDirectHandle` |
| Web | `@nostos-sync/web` | `Nostos…` |
| Node | `@nostos-sync/node` | `NostosClient`, `NostosDatabase`, `NostosHandle` |
| React Native | `@nostos-sync/react-native` | `NostosClient`, `NostosError`, `NostosPushError`, `NostosTurboModule` |
| Capacitor | `@nostos-sync/capacitor`, pod `NostosCapacitor`, `@CapacitorPlugin(name = "Nostos")` | `NostosPlugin`, `NostosSocket`, `NostosRow` |
| Tauri | crate `tauri-plugin-nostos`, JS `@nostos-sync/tauri` | `NostosSnapshot`, `NostosRaw`, `NostosWriteStatus` |
| Swift | SwiftPM package/product `Nostos` | `NostosClient` |
| Kotlin | package `run.nostos.sdk` (was `com.cairn.sdk`) | `NostosClient`, `NostosDatabase`, `NostosError` |
| .NET | NuGet `Nostos.DotNet`, namespace `Nostos` | `NostosClient`, `NostosException` |

Android namespaces: `dev.cairn.cairn_flutter` → `run.nostos.nostos_flutter`,
`com.cairn.reactnative` → `run.nostos.reactnative`. Tauri fixture identifier
`dev.cairn.fixture` → `run.nostos.fixture`.

## Config, env, files on disk

| today | nostos |
|---|---|
| `CAIRN_*` env (~90 vars: `CAIRN_PG_URL`, `CAIRN_SYNC_AUTH`, `CAIRN_PUSH_TABLES`, `CAIRN_E2E_PG` …) | `NOSTOS_*` (`NOSTOS_PG_URL`, `NOSTOS_SYNC_AUTH`, `NOSTOS_PUSH_TABLES`, `NOSTOS_E2E_PG` …) |
| `cairn.toml` | `nostos.toml` |
| `cairn_rules.toml` (ADR-0031) | `nostos_rules.toml` |
| `.cairn/` project dir (ADR-0023) | `.nostos/` |

> **Held (decision 2b).** The Postgres, wire and on-device sections below are
> the *eventual* names. Every row in them stays `cairn*` until its own
> migration ADR; the rename script keeps `--pg-identity`, `--wire-identity`
> and `--client-storage` off. The env and config rows above are renamed, and
> the old `CAIRN_*` / `cairn.toml` / `.cairn/` names keep being read as a
> fallback.

## Postgres identity (`--pg-identity`; needs `docker compose down -v`)

| today | nostos |
|---|---|
| db / user `cairn` | `nostos` |
| role `cairn_writer` | `nostos_writer` |
| publication `cairn_pub`, slot `cairn_slot` | `nostos_pub`, `nostos_slot` |
| tables `cairn_oplog`, `cairn_push_tokens` | `nostos_oplog`, `nostos_push_tokens` |
| schema `cairn.*` (`changes`, `push_templates`, `push_config`, `push_tokens`, `device_presence`, `current_scopes`, `log_change()`, `prune()`, `retention`, `push_cooldown`) | `nostos.*` |
| metrics `cairn_*` (e.g. `cairn_slot_recreated_total`) | `nostos_*` |

## Wire identity (`--wire-identity`; breaks old ↔ new peers)

| today | nostos |
|---|---|
| QUIC ALPN `cairn/sync/1` | `nostos/sync/1` |
| Tauri `plugin:cairn\|connect`, config key `plugins.cairn` | `plugin:nostos\|connect`, `plugins.nostos` |
| BroadcastChannel `cairn:multitab` | `nostos:multitab` |
| header `X-Cairn-Source` | `X-Nostos-Source` |
| WS path `/sync` | unchanged |

## On-device storage (`--client-storage`; resync or migrate)

| today | nostos |
|---|---|
| `cairn.db` | `nostos.db` |
| SQLite `cairn_data` / `cairn_meta` / `cairn_outbox` | `nostos_data` / `nostos_meta` / `nostos_outbox` |
| OPFS pool `cairn:opfs-sahpool` | `nostos:opfs-sahpool` |
| key prefix `cairn:checkpoint:<table>` | `nostos:checkpoint:<table>` |

## Ops

| today | nostos |
|---|---|
| image `cairn:latest` | `ghcr.io/unfazed-dev/nostos:latest` |
| containers `cairn-stack-{postgres,server,cloud,pushd}`, `cairn-postgres` | `nostos-stack-*`, `nostos-postgres` |
| `packaging/homebrew/cairn.rb`, `class Cairn < Formula` | `packaging/homebrew/nostos.rb`, `class Nostos < Formula` |
| release tarballs `cairn-<target>.tar.gz` | `nostos-<target>.tar.gz` |

## Open decisions (unchanged from the qairn inventory)

- ~~`--pg-identity`, `--wire-identity`, `--client-storage`: take or hold back~~ → held (decision 2b).
- Legal entity "Cairn Sync, Inc." in brand prose.
- npm scope: `@nostos-sync` (recommended) vs `@nostos-run`.
- pub.dev: `nostos_flutter` (mirrors today) vs bare `nostos`.
- Trademark search (USPTO/EUIPO) — not done.
