# qairn rename — inventory and decisions

Status: inventory, 2026-09-21. **Nothing here has been applied.** The tooling is
`scripts/rename-to-qairn.sh` (dry-run by default). Every count below was produced
by grepping the tracked tree, not estimated.

> Superseded 2026-09-24: the name is `nostos`, and `scripts/rename-to-qairn.sh`
> was deleted. The rename tool is `scripts/nostos_rename/` (see its README);
> the script is still in git history for reference.

Context: `docs/plans/naming-and-domain-2026-09-21.md` recommends `qairn` because
`cairn-cli` is already taken on crates.io at v0.1.6 and `cairn` itself is squatted
at v0.0.0 — the flagship crate name is permanently gone.

---

## 0. The shape of the problem

| | |
|---|---|
| tracked files (`git ls-files`) | 1254 |
| files containing `cairn` (any case) | **787** |
| total occurrences | **17,095** |
| paths containing `cairn` | **721** |
| top-most directories that must `git mv` | 25 |
| files whose basename also changes | 52 |
| distinct `CAIRN_*` environment variables | 123 |

Case shapes (disjoint under case-sensitive matching, so three substitutions cover
everything — `1900 + 4505 + 4183 + 3411 + 3096 = 17,095`):

| shape | occurrences | files | example |
|---|---:|---:|---|
| `CAIRN` | 1,900 | 268 | `CAIRN_E2E_PG`, `CAIRN_ACK_PROGRESS_INTERVAL` |
| `Cairn` | 4,505 | 437 | `CairnClient`, `CairnFlutterPlugin`, prose |
| `cairn_` | 4,183 | 494 | `cairn_flutter`, `use cairn_infra::`, `cairn_rules.toml` |
| `cairn-` | 3,411 | 499 | `cairn-domain`, `cairn-pushd`, `cairn-x86_64-…tar.gz` |
| bare `cairn` | 3,096 | 429 | the binary, `/cairn` db name, `cairn.dev` |
| — of which `dev.cairn*` | 48 | 21 | Android namespace, iOS/macOS bundle ids |
| — of which `@cairn/` | 166 | 64 | npm scope |

By area:

| area | occ | files | paths | decision |
|---|---:|---:|---:|---|
| `sdk/` | 6,023 | 230 | 367 | **rename** |
| `crates/` | 3,566 | 175 | 183 | **rename** |
| `docs/plans/` | 2,573 | 61 | 23 | **leave** (historical) |
| `docs/` (other) | 1,274 | 32 | 3 | **rename** |
| `docs/adr/` | 735 | 44 | 2 | **leave** (historical) |
| `apps/` | 680 | 50 | 6 | **rename** |
| `archive/` | 546 | 38 | 115 | **leave** (frozen; see `archive/README.md`) |
| root files | 528 | 31 | — | **rename** (Makefile, Cargo.toml, README, Dockerfile, fly.toml, .gitignore, .env.example) |
| `benches/results/` | 373 | 89 | 13 | **leave** (measurement record) |
| `web/` | 226 | 19 | 3 | **rename** |
| `docker/` | 141 | 8 | 1 | **split** — see §2 |
| `plans/` (top level) | 114 | 2 | 2 | **leave** (historical) |
| `.github/` | 102 | 2 | 0 | **rename** |
| `scripts/` | 91 | 3 | 0 | **rename** |
| `supabase/`, `deploy/`, `.zcode/` | 123 | 3 | 0 | rename / rename / leave |

The safe default sweep (`scripts/rename-to-qairn.sh` with no flags) selects 900
files, changes 529 of them, and leaves 354 files plus five opt-in token
categories alone.

---

## 1. What the default sweep does

Renames, in content and in paths:

- All 12 workspace crates + `cairn-cloud` (`crates/cairn-*` → `crates/qairn-*`),
  and every `use cairn_infra::` style import that follows from it.
- All 9 SDK directories (`sdk/cairn_flutter`, `_swift`, `_kotlin`, `_node`,
  `_web`, `_tauri`, `_dotnet`, `_capacitor`, `_react_native`).
- The npm scope `@cairn/*` (166 occ / 64 files). **Unclaimed on npm** per the
  naming doc, so this is a claim-to-be-made, not a live thing that breaks.
- All 123 `CAIRN_*` env vars and the Make variables built on them.
- Binary names `cairn`, `cairn-server`, `cairn-cloud`, `cairn-pushd`; the
  Dockerfile stages and `ENTRYPOINT`; the `fly.toml` app name `cairn-sync`.
- CI job names and `working-directory:` paths in both workflows.
- Type names across Dart/Swift/Kotlin/C#/TS (`CairnClient`, `CairnHandle`,
  `CairnFlutterPlugin`, `CairnDatabase`, …).
- Forward-looking prose in `README.md`, `docs/` (excluding ADRs/plans),
  `CONTRIBUTING.md`, `SECURITY.md`, `CONTEXT.md`, `AGENTS.md`, `CLAUDE.md`.
- `.gitignore` patterns — these must move with the directories or the renamed
  build outputs stop being ignored.

---

## 2. What must NOT be blindly renamed

These are the categories a naive `sed s/cairn/qairn/g` gets wrong. Each is an
**opt-in flag**, default OFF.

### 2.1 Postgres identity — `--pg-identity` (375 occ / 65 files)

Held back: the whole DSN `postgres://…`, `POSTGRES_USER|PASSWORD|DB: cairn`,
`psql -U cairn -d cairn`, `pg_isready -U cairn -d cairn`, the least-privilege
role `cairn_writer` / `cairn_writer_dev_pw`, and the SQL object names
`cairn_pub`, `cairn_oplog`, `cairn_slot`, `cairn_push_tokens`, `idx_cairn_*`.

Four things carry the name and they are **not** all the same thing:

| thing | where | owned by |
|---|---|---|
| role + password + db name `cairn` | `docker/docker-compose.yml`, `docker-compose.stack.yml`, `Makefile:106`, `.github/workflows/ci.yml:92`, `CLAUDE.md` | the docker volume `pgdata` |
| least-privilege role `cairn_writer` | `docker/pg-init/02-cairn-role.sql`, `Makefile:22` | the initdb script |
| publication `cairn_pub`, slot `cairn_slot` | `docker/pg-init/01-sources.sql`, Makefile readiness poll, CI readiness poll, `CAIRN_PG_*` defaults, 20+ e2e tests | the running database |
| internal tables `cairn_oplog`, `cairn_push_tokens` | `01-sources.sql`, `crates/cairn-infra/src/oplog.rs`, `crates/cairn-push` | the running database |

**Decision: needs-human-call, but all-or-nothing.** These four move together or
none of them does. `pg-init/*.sql` runs **only on first init** of an empty
`pgdata` volume — so renaming the SQL without `docker compose down -v` leaves a
database that still has `cairn_pub` while the code now asks for `qairn_pub`.

**Failure mode of getting it half-right**, concretely:

- Rename the compose env but not `pg-init`: the container boots with role/db
  `qairn` and `02-cairn-role.sql` runs `GRANT … TO cairn_writer` against tables
  it can see — but `Makefile:110`'s `psql -U cairn -d cairn` slot-cleanup step
  can no longer connect, so leaked `e2e_*` slots are never dropped and the next
  `make pg-e2e` dies with "all replication slots are in use" (the exact failure
  from 2026-09-21, 7 spurious failures).
- Rename `cairn_pub` in the SQL but reuse the existing volume: `pg-init` does
  not re-run, the publication is still `cairn_pub`, and the Makefile readiness
  poll (`SELECT 1 FROM pg_publication WHERE pubname='qairn_pub'`) spins for its
  full timeout. CI's equivalent poll at `ci.yml:74` does the same — a 10-minute
  red job with no useful error.
- Rename `cairn_oplog` in `oplog.rs` but not in `01-sources.sql`: reconnect
  resume silently reads an empty table. That is the P0-1 data-loss class.

**Recommendation:** take `--pg-identity`, and in the same commit run
`docker compose -f docker/docker-compose.yml down -v` so `pg-init` re-runs
against a fresh volume. Then `make pg-e2e` before believing anything.
Alternatively: leave the DB identity at `cairn` forever and treat it as an
internal-only name. That is defensible but means `qairn-server` connects as
`cairn_writer`, which will confuse every future reader.

### 2.2 On-the-wire protocol identifiers — `--wire-identity` (61 occ / 17 files)

Held back:

| identifier | where | why it is not a string |
|---|---|---|
| `cairn/sync/1` | `crates/cairn-infra/src/iroh_sync.rs:44` (`CAIRN_SYNC_ALPN`), ADR-0041 | QUIC ALPN is **negotiated**. Both peers must offer the same bytes or the connection is refused outright. |
| `plugin:cairn\|connect` | `sdk/cairn_tauri` | Tauri invoke names. The plugin registration and every JS call site must change in lockstep. |
| `plugins.cairn` | `sdk/cairn_tauri/fixture/tauri.conf.json` | Config key Tauri deserializes by name — an existing app's `tauri.conf.json` breaks with "Error deserializing 'plugins.cairn'". |
| `cairn:multitab` | web SDK | BroadcastChannel name; two tabs on different versions stop seeing each other. |

**Decision: rename, but only at a version boundary** where server and every SDK
ship together. Pre-1.0 with no external deployments, that is now — take the flag.
If any third party has a client running, do not.

### 2.3 On-device storage names — `--client-storage` (609 occ / 121 files)

Held back: `cairn.db`, `cairn.sqlite`, `cairn.toml`, the client SQLite tables
`cairn_data` / `cairn_meta` / `cairn_outbox`, the OPFS pool name
`cairn:opfs-sahpool`, the storage key prefix `cairn:checkpoint:<table>`, the
`.cairn/` project directory (ADR-0023) and `cairn_rules.toml` (ADR-0031).

Renaming any of these **orphans data that already exists on a device**: the app
opens `qairn.db`, finds it empty, and re-syncs from scratch — or worse, opens it
and finds no `qairn_meta` row, so it has no `resume_lsn` and replays everything.
The OPFS pool name is the same problem in a browser. `.cairn/` and
`cairn_rules.toml` are the same problem in a **user's repository** — they are
the CLI's on-disk contract, and `cairn rules init` writing `qairn_rules.toml`
next to an existing `cairn_rules.toml` is a silent config-ignored bug.

**Decision: needs-human-call.** Either (a) rename and accept a one-time full
resync + tell users to `mv cairn_rules.toml qairn_rules.toml`, or (b) ship a
migration: open the old name, `ALTER TABLE cairn_data RENAME TO qairn_data`,
fall back to `cairn_rules.toml` when `qairn_rules.toml` is absent. Given
pre-1.0 and no external users, (a) is the honest cheap answer — but it is a
decision, not a substitution.

### 2.4 External identifiers already published — `--external-ids` (92 occ / 40 files)

| identifier | count | live, or a claim to be made? |
|---|---:|---|
| `https://github.com/unfazed-dev/cairn` | 27 (+ 12 sub-paths) | **LIVE.** Renaming the string does not rename the repo. GitHub redirects after a repo rename, but `Cargo.toml:63` `repository =` and the `.git` remote both need the real rename first. |
| `unfazed-dev/homebrew-cairn` tap | 2 | **LIVE-ish.** A separate repository; renaming the string here does nothing to it. |
| release assets `cairn-<triple>.tar.gz` under `/releases/download/v0.2.0/` | 5 in `packaging/homebrew/cairn.rb`, more in `packaging/release/fill_prebuilt_manifest.py` | **LIVE.** Those files exist on the v0.2.0 release with those exact names. Renaming the formula URLs makes `brew install` 404. Formula URLs must keep pointing at old assets until a new tag is cut. |
| tags `v0.1.0`, `v0.2.0`, `pre-ads-move-2026-09-02` | 3 | **Immutable.** Not touched by this script; nothing should retag. |
| `dev.cairn.*` bundle ids | 48 / 21 files | **Depends.** Android `applicationId` / iOS bundle id are the app's store identity. Nothing has shipped to a store (no fastlane release record in-repo), so these are claims-to-be-made → safe to rename. Once a build is in TestFlight or Play, an applicationId change is a **new listing with no upgrade path for existing installs.** |
| crates.io `cairn-*` names | — | **Not ours.** `cairn`, `cairn-core`, `cairn-cli` are taken by strangers; `qairn*` is free everywhere. Pure claim-to-be-made — this is the entire reason for the rename. |
| npm `@cairn` scope, pub.dev `cairn_flutter` | 166 / 64 | **Unclaimed.** Claims-to-be-made. Renamed by the default sweep. |
| `cairn.dev`, `getcairn.io`, `founders@cairn.dev`, `*@cairn.dev` seed rows | ~25 | **Unowned** (the naming doc concludes no domain is needed to launch). Renamed by the default sweep. `cairn.example.com` is a doc placeholder — also renamed. |

**Decision: take `--external-ids` only in the commit where you also (1) rename
the GitHub repo, (2) rename the tap repo, and (3) cut a new tag whose assets are
named `qairn-<triple>.tar.gz`.** Until then the strings must keep naming things
that actually exist.

### 2.5 Historical documents — `--historical-docs` (354 files excluded)

`docs/adr/` (735 occ / 44 files), `docs/plans/` (2,573 / 61), `plans/` (114 / 2),
`benches/results/` (373 / 89), `archive/` (546 / 38), `.zcode/` (42 / 1).

ADRs record decisions made, and `benches/results/RESULTS.md` records measurements
taken, under the name Cairn. Rewriting them retroactively falsifies the record —
a reader in six months cannot tell whether ADR-0013 actually said "qairn-server"
in 2026-07 or whether a script said it did. `benches/results/**` additionally
contains machine-generated JSON with absolute paths from the measuring host
(`/var/folders/…/cairn-apply-bench-85393/client-9.db`); editing those makes the
artifacts no longer match what the harness emitted.

**Policy (recommended):**

1. Leave every ADR **body** and filename untouched. Two ADR filenames contain
   the name — `0008-visual-identity-the-cairn-field.md` and
   `0023-dot-cairn-project-directory-and-backend-adapters.md` — and both are
   cited by path from code and docs; renaming them breaks those citations for no
   gain.
2. Leave `benches/results/**` untouched, including `RESULTS.md` (36 occurrences).
   A benchmark record is a measurement, and its subject had a name at the time.
3. Leave `docs/plans/`, `plans/`, `.zcode/` — dated plan documents, same logic.
4. Leave `archive/` — `archive/README.md` already states nothing in it is built,
   tested, linted, published or run. 115 of its paths carry the name; moving
   them is churn with zero effect.
5. **Add exactly one note.** There is no `docs/adr/README.md`, so this should be
   a new ADR in the sequence (next free number), body in one paragraph: *"The
   project was renamed cairn → qairn on &lt;date&gt; (see
   docs/plans/naming-and-domain-2026-09-21.md). Documents dated before that read
   'Cairn'; that is the name the decision was made under and has not been
   rewritten."* One note beats 4,000 retroactive edits.

Accepted cost: plan docs will reference `crates/cairn-infra`, a path that no
longer exists. That is what a dated document is supposed to do.

### 2.6 Brand-metaphor prose — `--brand-prose` (10 occ / 5 files)

A rename makes these sentences false, not just stale. "A qairn is a pile of
stones that marks a trail" is not a statement about anything.

| file:line | text |
|---|---|
| `README.md:237` | "A cairn is a pile of stones that marks a trail… **Sync checkpoints (LSNs) are our cairns**" |
| `docs/STRATEGY.md:4` | "A cairn is a trail marker of stacked stones… Cairn is how your data does." |
| `docs/STRATEGY.md:141-143` | the §4 name rationale: "**karn/** — a pile of stones marking a trail", plus the domain shortlist `cairn.dev / getcairn.io / cairnsync.com` |
| `crates/cairn-cloud/static/landing.html:182` | the same sentence in the shipped landing-page footer, next to "© 2026 Cairn Sync, Inc." |
| `crates/cairn-cloud/static/tokens.css:8-11` | the design-token header: "Brand idea: a cairn is a stack of stones marking a trail" — and the entire `--stone-*` palette derives from it |
| `docs/adr/0008-visual-identity-the-cairn-field.md` | the whole ADR is the visual identity built on the metaphor (already excluded as historical) |

**Decision: needs-human-call — rewrite, do not substitute.** The name `qairn`
has no established meaning, which is exactly the argument the naming doc makes
in its favour (it owns its search results); but it means the origin story has to
be written fresh. `docs/STRATEGY.md §4` and the landing page need a human.
`tokens.css`'s `--stone-*` variable names are unaffected either way — they are
colours, and nothing named `cairn` appears in them beyond the comment.

Also note `crates/cairn-cloud/static/landing.html` carries **"Cairn Sync, Inc."**
— a legal entity name, not a product name. That is a separate decision from the
software rename.

### 2.7 Lockfiles — `--lockfiles` (17 files)

`Cargo.lock` (38 occ), 5 SDK-local `Cargo.lock`s, 3 `pubspec.lock`, 4
`package-lock.json`.

**Decision: leave; regenerate.** Hand-editing a lockfile produces a file whose
checksums no longer match its contents. After `--apply`, run
`cargo metadata --offline`, `flutter pub get`, `npm install` and commit the
result. The flag exists only so an operator can see the counts.

---

## 3. Other things a naive sweep breaks

- **`apps/atlet/flutter/web/cairn/cairn_ffi_wasm_bg.wasm`** — the only tracked
  binary containing the name. `sed` cannot reach it; the script skips binaries.
  It must be rebuilt (`wasm-pack build crates/cairn-ffi-wasm --target web`,
  `Makefile:153`) and re-copied.
- **`sdk/cairn_flutter/rust`** is a **non-member** crate (ADR-0015 addendum, the
  flutter_rust_bridge codegen with the one permitted `unsafe`). It has its own
  `Cargo.lock` and its generated glue is full of
  `wire__crate__api__cairn__CairnHandle_*` symbol names. The rename is fine
  textually, but the generated file should be **regenerated**, not patched, or
  the next codegen run produces a conflicting diff.
- **Replication slot prefixes.** The e2e tests build slot names like
  `e2e_pg_writeback_<pid>` and `repro_*` — these do **not** contain "cairn" and
  are untouched, which is correct. `cairn_slot` (the app slot, 65 occ) and
  `cairn_pub` (139 occ) do, and are held back with `--pg-identity`. `make pg-e2e`
  drops leaked inactive `e2e_*`/`repro_*` slots and *deliberately leaves*
  `cairn_slot` and `atlet_*` alone (`Makefile:103`) — that cleanup filter must
  be updated in the same commit as the slot rename or it starts dropping the app
  slot.
- **Docker container names** `cairn-postgres`, `cairn-stack-postgres`,
  `cairn-stack-server/cloud/pushd` and the image tag `cairn:latest`. Renamed by
  the default sweep. A running stack keeps the old container names until
  `docker compose down` — expect one round of "container name already in use".
- **CI readiness polls.** `ci.yml:74-79` and `Makefile:126-139` poll for
  `pg_publication WHERE pubname='cairn_pub'`. Covered by `--pg-identity`; if that
  flag is taken, the poll and `pg-init` must change together (see §2.1).
- **`.gitignore`** references `sdk/cairn_react_native/…` and
  `libcairn_swift.a` — renamed by the default sweep, which is required, or the
  moved artifacts stop being ignored.
- **Homebrew formula class name.** `packaging/homebrew/cairn.rb` defines
  `class Cairn < Formula`; the class name must match the filename
  (`qairn.rb` ⇒ `class Qairn`). The script renames both — but the formula lives
  in the *tap* repo, and only the template is here.
- **No `LICENSE` or `NOTICE` file exists** at the repo root. `license =
  "Apache-2.0"` in `Cargo.toml` is an SPDX id and carries no project name, so
  there are no license headers to rewrite. (Worth fixing separately — an
  Apache-2.0 project should ship a LICENSE file.)
- **`cairnincr` / `cairnorset` / `cairncountertenant`** — PG test fixture tables
  with no separator. The lowercase rule handles them correctly
  (`qairnincr`, …); listed here only because they are easy to miss when eyeballing
  a `cairn_`/`cairn-` diff.
- **The `cairn` CLI UX itself.** `cairn rules init|edit|check`, `cairn dev`,
  `cairn doctor`, `cairn deploy`, `cairn pull && cairn gen` appear in generated
  file headers (`// GENERATED by \`cairn gen\``) and in error strings. Renamed by
  the default sweep — deliberately, since the whole point of the rename is that
  `cargo install qairn-cli` should install *our* binary.

---

## 4. Order of operations

The script does **content first, paths second**, and that order is load-bearing:
`git ls-files` is enumerated once, so moving `crates/cairn-infra/` first would
carry 59 not-yet-rewritten files out from under the iterator. Renaming after the
rewrite is safe because `git mv` does not read file contents.

Suggested sequence:

1. `./scripts/rename-to-qairn.sh` — read the dry run.
2. `./scripts/rename-to-qairn.sh --apply` — the safe sweep only.
3. Regenerate lockfiles; rebuild wasm; regenerate flutter_rust_bridge glue.
4. `make ci`. Expect it to be green at this point — nothing in the safe sweep
   touches a running database or an on-disk contract.
5. Decide §2.1 / §2.3 / §2.4 explicitly, each as its own commit with its own
   flag, each followed by its own verification (`make pg-e2e` for §2.1).
6. Rewrite the §2.6 prose by hand.
7. Add the one §2.5 note. Do not touch the ADRs.

The sweep is idempotent — `qairn` contains no `cairn`, so a second run reports
zero changes and zero moves.
