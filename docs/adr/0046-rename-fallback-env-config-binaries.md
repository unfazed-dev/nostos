---
adr_decision:
  hard_to_reverse: false
  reversal_cost: "Low. The fallback is one helper module, one path resolver, one packaging script and a handful of held constants. Deleting them at 1.0 is the plan; deleting them earlier strands deployments that still set the pre-rename env vars or keep the pre-rename config files."
  surprising_without_context: true
  surprise_reason: "Constants whose new and legacy values are identical, a clippy ban on std::env::var, a clap wrapper that rewrites env attribute names, and packaging steps that do nothing. All of it is inert until the project rename lands, and without this record it reads as dead code."
  result_of_real_tradeoff: true
  rejected_alternatives: "Copy legacy env values into the new names with std::env::set_var at startup (races concurrent readers under tokio and is unsafe from edition 2024); hard-cut the names at the rename (breaks every deployed .env, unit file and nostos.toml at once); rename config files on disk automatically (a tool silently moving a committed file is a surprise in the operator's git diff); keep the old names forever (the fallback is a migration aid, not a second contract)."
  all_three_true: false
status: accepted
---

# ADR-0046: Pre-rename env, config and binary names keep working until 1.0

- **Status:** Accepted (2026-09-24).
- **Date:** 2026-09-24
- **References:** `docs/plans/nostos-name-map-2026-09-24.md` (the name map and
  the held rows), `docs/ci/decisions.md` decision 10 (fallback lifetime),
  ADR-0023 (the project dir), ADR-0031 (the rules file), ADR-0038 (the push
  daemon).

## Context

The project rename is mechanical: every `cairn` becomes the new name, every <!-- rename:hold -->
`CAIRN_` the new env prefix. Operators, though, have `CAIRN_*` in `.env` files, <!-- rename:hold -->
unit files and fly secrets, a `cairn.toml` / `cairn_rules.toml` / `.cairn/` <!-- rename:hold -->
committed next to their app, and scripts that call `cairn-server`. A hard cut <!-- rename:hold -->
breaks all of them on upgrade. Decision 10 fixes the lifetime: the old names
keep working until the first major (1.0), with a one-time deprecation warning.

The layer lands **before** the rename. The rename copies any line that
contains the marker `rename:hold` verbatim, so each legacy name sits on its
own held line, next to the constant it falls back to:

```rust
pub const PREFIX: &str = "NOSTOS_";
pub const LEGACY_PREFIX: &str = "CAIRN_"; // rename:hold — …
```

Today both values are equal and every fallback is a guarded no-op. After the
rename the first line reads the new name and the second still reads the old
one.

Two rules keep the held lines stable through the rename. First, identifiers
declared on a held line never contain the old name (`LEGACY_PREFIX`, not
`LEGACY_CAIRN_PREFIX`), because the non-held lines that use them would be <!-- rename:hold -->
renamed and stop matching. Second, prose around the layer says "the project
rename", never "old → new", which the rename would turn into "new → new".

## Decision

### 1. Env: one reader, no `set_var`

`nostos_infra::env` is the only sanctioned process-env reader:

- `var` / `var_os` are drop-ins for `std::env`. A name that carries `PREFIX`
  and is unset falls back to its `LEGACY_PREFIX` spelling.
- `parse::<P: clap::Parser>()` is a drop-in for `Parser::parse`. It walks the
  clap `Command` (subcommands too) and repoints each `env = "…"` attribute at
  the legacy var when only that one is set. Clap keeps its own precedence
  (flag > env > default), bool parsing and `hide_env_values`.
- `fold_legacy` does the same for whole-map reads: a parsed `.env` and the
  `nostos push` env snapshot.

**No `std::env::set_var`.** Copying the legacy value into the new name at
startup is the obvious shortcut, and it is wrong here. It races every other
thread reading the environment (tokio workers, the `tracing` filter, rustls
reading `SSLKEYLOGFILE`), and from edition 2024 it is `unsafe`, which this
workspace forbids.

`clippy.toml` bans `std::env::var`, `var_os`, `vars`, `vars_os` and
`clap::Parser::parse`, so a new direct read fails `make ci` instead of silently
losing the fallback. The two sanctioned exceptions carry
`#[allow(clippy::disallowed_methods)]` with a reason: the helper itself and
the prefix scan in `nostos push`.

The warning is `eprintln!`, once per legacy name, not `tracing`: clap
resolves env before the subscriber exists, and `NOSTOS_LOG` is itself one of
those args.

### 2. Config files and dirs: resolve once, edit in place

`nostos_infra::config_path::resolve(dir, name, legacy)` returns the primary
path when it exists, the legacy path when only that one exists, and the
primary path when neither does. Readers and writers both call it, so a legacy
file is edited in place and a second copy never appears. Picking the legacy
path warns once. Covered:

| pre-rename name | where it resolves |
|---|---|
| `cairn_rules.toml` | `rules_file::path_in`. The server resolves its default once at boot, so the boot load, the reload poll and `PUT /rules` all share one path. The cli uses the same helper. | <!-- rename:hold -->
| `cairn.toml` | `config::config_path`, every cli command | <!-- rename:hold -->
| `.cairn/` | `config::dot_dir`: `ProjectConfig` load and save, `link`, `pull`, `gen`, and the `.gitignore` entry (a legacy `.cairn/local/` holds the same secrets) | <!-- rename:hold -->
| `./cairn-pushd.db`, `cairn-cloud.db` | the pushd and cloud defaults, so a default-path deployment keeps its registry | <!-- rename:hold -->
| `assets/cairn.json` | the Flutter SDK's config loader, which falls back on a `FlutterError` | <!-- rename:hold -->

The server's reload poll warns about a missing rules file only on the
present → missing transition. A zero-config deploy has no file at all
(`all` mode), and warning on every poll there was noise.

Explicit paths (`--rules-file`, `NOSTOS_PUSHD_DB`, …) are taken as given, with
no fallback.

### 3. Binaries: symlinks, the Valkey pattern

`packaging/legacy-binary-names.sh DIR` symlinks `cairn`, `cairn-server`, <!-- rename:hold -->
`cairn-pushd` and `cairn-cloud` to their renamed binaries in `DIR`. It is <!-- rename:hold -->
guarded: no self-link while the names are equal, it never replaces an
existing file, and it skips any binary `DIR` does not ship. It runs in the
Dockerfile runtime stage and in the release tarballs (`release.yml`, macOS
and Linux). The Homebrew formula does the same with `bin.install_symlink`.

ponytail: the Windows zip ships no legacy names, because symlinks there need
admin rights or developer mode. Add `.exe` copies if a Windows user asks.

### 4. Non-Rust reads

- **Fallback added.** These are operator- or user-facing runtime reads:
  - the Supabase push function's `NOSTOS_PUSH_SECRET` (silent: it runs once
    per request);
  - the Flutter build hook's `NOSTOS_FLUTTER_CARGO_FEATURES` (silent);
  - `sdk/nostos_react_native/scripts/build-ios-staticlib.sh`'s `NOSTOS_PROFILE`
    (warns);
  - the Flutter config asset (warns via `debugPrint`).
- **Not needed.** Scripts and config that set their own vars: the Makefile,
  CI workflows, docker-compose files, `fly.toml`, the Dockerfile `ENV`, and
  SDK/app test scripts. Also compile-time `String.fromEnvironment` in
  first-party apps and the web dev proxy.
- **Held by the name map, so not renamed at all.** The on-device storage
  names (`cairn.db`, SQLite tables, OPFS) and the wire and Postgres identity. <!-- rename:hold -->

## Consequences

- Today nothing changes: every fallback compares two equal names and skips.
- After the rename, `NOSTOS_*` and the new file names are primary and the old
  ones keep working with a warning. Both set means the primary wins.
- Removal at 1.0: delete the `LEGACY_*` constants and the held lines, then
  delete whatever stops compiling. The helper API (`env::var`, `env::parse`,
  `config_path::resolve`) can stay as thin wrappers or be inlined.
- The clippy ban outlives the fallback only if it keeps earning its place.
  Drop it together with the helper.
