# nostos rename — how to do it without breaking anything (research, 2026-09-24)

The goal: after the rename, everything that works with cairn today keeps
working unchanged. That covers the live Supabase direct-mode project, the atlet
phone pilot, arxa-studio (its pinned SHAs, desktop binaries and env), local dev
and CI. Name map: `nostos-name-map-2026-09-24.md`. Decisions:
`docs/ci/decisions.md`.

## 1. What a naive rename breaks

| surface | who depends on it | effect of a naive rename |
|---|---|---|
| SHA `224ccef…` (arxa-studio `plugins/cairn-rail/lib/adapter.js:27`, asserted in `selftest.mjs:146`) and `fabc1a16…` (arxa-studio `mobile/pubspec.lock`, via archived `arxa.git` `kit/cairn`) | arxa-studio | the history rewrite leaves the SHAs unreachable, so `pub get` and the selftest fail |
| PG schema `cairn.*`, RPCs `cairn_pull/increment/snapshot/ring_read/changes_read/changes_ring`, triggers `cairn_log_*` (`.cairn/direct.sql`, generated from `crates/cairn-cli/src/direct.rs`) | live project `ltamqsxxumtusyxswezi` + atlet builds | clients call RPCs that no longer exist. `ALTER SCHEMA … RENAME` also breaks PL/pgSQL bodies, which are stored as source text and parsed at first call. A `nostos link --deploy` would install a second schema that no client reads |
| applied migrations `apps/atlet/supabase/migrations/0001–0008` | same | editing applied files makes a fresh `db reset` differ from prod |
| Edge Function `cairn-push`, payload `{cairn: 'ring'}` (`push_pilot.dart:45`) | installed atlet builds | old apps ignore the new payload key |
| on-device `cairn_direct.sqlite`, `cairn.db`, tables `cairn_data/meta/outbox`, OPFS pool `cairn:opfs-sahpool`, keys `cairn:checkpoint:*` | phones/browsers holding data | a new name opens an empty db: a full resync, and unsynced **outbox rows are orphaned (data loss)** |
| slot `cairn_slot`, publication `cairn_pub` | running server | a slot can't be renamed (only FAILOVER/TWO_PHASE can be altered), so a new name means a new slot and a full resnapshot |
| QUIC ALPN `cairn/sync/1`, Tauri `plugin:cairn\|…` / `plugins.cairn`, BroadcastChannel `cairn:multitab`, header `X-Cairn-Source` | old ↔ new builds, open tabs | handshake or IPC mismatch |
| binaries `cairn-server`, `cairn-pushd`, `cairn`; `cargo install --path crates/cairn-server` | arxa-studio desktop (`cairn_server.rs`, `pushd.rs`), `plugins/push-doorbell` | the binary lookup fails |
| ~90 `CAIRN_*` env vars (36 direct `std::env::var` reads) | arxa-studio desktop, `.env`s, CI, docker | vars become silently unset, so defaults apply |
| `cairn.toml`, `cairn_rules.toml`, `.cairn/` | existing projects, including the live one | config is ignored and the project falls back to zero-config `all` mode |
| tags `v0.1.0` `v0.2.0` `pre-ads-move`, PR #1/#2 | GitHub | they point at pre-rewrite commits |

**Not at risk:**
- Nothing is published: 0 artifacts on crates.io, npm, pub, brew or ghcr.
- No outside workflow `uses:` the repo.
- No Pages site.
- 0 stars, 0 forks.

## 2. What the docs say

- **Parallel Change** (expand → migrate → contract): add the new name beside the old one, and remove the old one only after every consumer has moved. <https://martinfowler.com/bliki/ParallelChange.html>
- **Renamed forks keep the old contract.**
  - OpenTofu still reads the `TF_*` env vars and `.terraformrc` (`.tofurc` wins). <https://opentofu.org/docs/cli/config/config-file/>
  - Valkey installs `redis-*` symlinks by default (`USE_REDIS_SYMLINKS`) and still reports `redis_version`. <https://github.com/valkey-io/valkey>
- **Rust:** renaming a public item is a *major* change; the fix is a `#[deprecated] pub use` alias. It only matters for unpinned consumers, and we have none. <https://doc.rust-lang.org/cargo/reference/semver.html>
- **Postgres.**
  - A schema rename breaks function bodies. <https://www.postgresql.org/docs/current/plpgsql-implementation.html>
  - `ALTER_REPLICATION_SLOT` changes only failover/two_phase. <https://www.postgresql.org/docs/current/protocol-replication.html>
  - `pg_copy_logical_replication_slot` copies a slot at the same LSN, if a rename is ever needed. <https://www.postgresql.org/docs/current/functions-admin.html>
- **ALPN:** the client offers a list and the server picks one, so both names can coexist on the wire later. <https://www.rfc-editor.org/rfc/rfc7301>
- **History rewrite.**
  - "Rewriting a branch that others have based work on is a bad idea." <https://git-scm.com/docs/git-rebase>
  - filter-repo produces a new history that is incompatible with the old one; run it on a fresh `--no-local` clone. <https://github.com/newren/git-filter-repo/blob/main/Documentation/git-filter-repo.txt>
- **GitHub repo rename:** redirects git and web traffic, but not Actions `uses:`. Creating a new repo at the old name kills the redirect. <https://docs.github.com/en/repositories/creating-and-managing-repositories/renaming-a-repository>
- **pub git deps:** pub keeps a `git clone --mirror` cache (`dart-lang/pub` `lib/src/source/git.dart:880`). A mirror fetches **every ref**, so a SHA reachable from any tag still resolves. <https://dart.dev/tools/pub/dependencies#git-packages>
- **GitHub limits:** content creation is capped at 80/min and 500/hr. PRs can be closed but never deleted, and the create-PR API has no date field. <https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api>
- **git-cliff:** builds changelogs from conventional commits, with PR metadata. <https://git-cliff.org/docs/>

## 3. Strategy: three buckets

**A. Hold.** Bytes stay unchanged until a dedicated migration ADR. This covers everything persisted or on the wire:
- the PG schema, RPCs, triggers, tables, publication, slot, role and db user;
- the applied migrations, plus `direct.sql` and the SQL inside `direct.rs`;
- the `cairn-push` Edge Function and the push payload key;
- SQLite file and table names, the OPFS pool and checkpoint keys;
- the ALPN, the Tauri plugin id and config key, the BroadcastChannel and `X-Cairn-Source`;
- metric names and the docker db credentials.

These are invisible to users, and renaming them is a data migration, not a rename. Each hold site gets a `ponytail:` comment naming the upgrade path: ALPN dual-offer, slot copy, schema expand/contract.

**B. Rename, and keep reading the old name** (the OpenTofu pattern). This is for operator-facing inputs:
- **env:** one helper reads `NOSTOS_X`, then falls back to `CAIRN_X` with a one-time deprecation warning. All 36 reads go through it. Mirroring the vars at startup with `set_var` is rejected: it races under `#[tokio::main]` and becomes `unsafe` in edition 2024.
- **files:** read `nostos.toml` / `nostos_rules.toml` / `.nostos/` first, then fall back to `cairn.toml` / `cairn_rules.toml` / `.cairn/`. The live project's `.cairn/config.json` keeps working.
- **binaries:** release tarballs and the brew formula also ship `cairn-server`, `cairn-pushd` and `cairn` symlinks (the Valkey pattern).

**C. Rename outright.** This covers build-time names with no unpinned consumer:
- crates, packages and dirs;
- Rust, Dart, TS, Swift, Kotlin and .NET types;
- docs and CI.

No type aliases: nothing is published, and every consumer pins a SHA. On the consumer side, one scripted arxa-studio update does the rest:
- look up `nostos-server` first, then `cairn-server`;
- look for `crates/nostos-server` first, then `crates/cairn-server`;
- update the env names.

## 4. Rewriting history without breaking pins

1. **Before** rewriting, keep the old repo untouched as the source of truth:
   - Take an offline backup: `git clone --mirror` plus `git bundle create cairn-full.bundle --all`. This copy doesn't depend on GitHub.
   - Rename `unfazed-dev/cairn` → `unfazed-dev/cairn-archive`, then archive it. It becomes read-only, and unarchiving is always possible.
   - GitHub redirects `unfazed-dev/cairn.git` to the archive, so the pub pin `fabc1a16` resolves from the untouched history.
   - `PINNED_CAIRN_COMMIT` (`224ccef`) is only a constant asserted in the selftest. Nothing fetches it, so it can't break.
   - Nothing needs migrating: the repo has 0 secrets, vars, envs, hooks, deploy keys, releases or protection rules. Its 2 PRs and 0 stars stay with the archive.
   - Rollback: delete `nostos`, unarchive `cairn-archive` and rename it back.
2. Rewrite on a fresh `--no-local` clone and push the result to a **new** `unfazed-dev/nostos` repo.
   - No force-push, no archive tags, no doubled objects.
   - Retro PRs are numbered #1…#N in historical order. A renamed repo would continue from #3.
   - Identity → noreply.
   - Content, paths and messages go through **the same rule table as the forward script**, holds included. A plain `--replace-text cairn==>nostos` would rename held identifiers in every historical blob.
   - `commit-map` drives the fixup of the 228 short hashes.
3. Bumping the arxa-studio pins then becomes optional: a scripted commit-map lookup.
4. Never create a new `unfazed-dev/cairn` repo: that kills the redirect, and the pub pin depends on it.

## 5. Retro PRs: what GitHub allows

- **Dates:** PR pages show the day they were created. The historical date goes in the title and body, and on the merge commits (git dates can be set).
- **Merge status:** a PR is marked *merged* when its head SHAs become reachable from base, including through a direct push. So push the branches, open the PRs, then force-push the rewritten `main`.
- **Pacing:** create the PRs paced under 80/min and 500/hr. They are permanent.
- **Summaries:** git-cliff generates them into `docs/ci/retro/*.md`, and they double as the PR bodies.
- **Two options:**
  - **A:** merge topology plus the retro docs, with zero API writes.
  - **A+R:** the same, plus real PR pages.

## 6. Verification gate: "nostos works like cairn"

1. **Baseline on the old tip:** run `make ci`, `make pg-e2e`, sdk-e2e, supabase-e2e, web-conformance, and `make bench` ×3. Save the pass counts and medians.
2. **Reverse-map diff:** map the new tree nostos→cairn and diff it against the old tree. The only allowed diffs are the bucket-B fallback code. This proves the rename was mechanical.
3. **Held-token census:** the count of every held identifier is the same before and after.
4. **Fallback tests:** env set as `CAIRN_X` only, `NOSTOS_X` only, and both (nostos wins); config in `.cairn/` only and `.nostos/` only.
5. **Same gate on the new tip:** identical pass counts, and the bench median within the spread of the old runs.
6. **Rewrite check:** `git rev-parse HEAD^{tree}` on the rewritten tip equals the forward-rename tip. That shows the product is byte-identical. The retro docs land in a later commit.
7. **Downstream:** the arxa-studio selftest passes with the **old** pins (redirect + archive tag), and again after the pin bump. An atlet push smoke test against the unchanged Supabase project passes.

## 7. Rollout order

1. **In a worktree:** run the forward rename script (holds are the default), add the fallback helper and its tests, then run gates 1–5.
2. **On a fresh clone:** run the rewrite, then gate 6.
3. **User pushes:**
   1. take the mirror + bundle backup;
   2. rename `cairn` → `cairn-archive` and archive it;
   3. create `unfazed-dev/nostos` and push `main` and the tags;
   4. optionally, open the R PRs.
   - Work continues in a new `nostos/` clone. Claude's per-project memory is keyed by folder, so it starts fresh there; the in-repo CLAUDE.md carries over.
4. **arxa-studio:** run the scripted update, then gate 7.

No step touches the live Supabase project.

## 8. After the rename: restructure + cleanup backlog (audit 2026-09-24)

These land as ordinary forward `[arxa-*]` PRs in `nostos`, each going through CI. None of them is part of the rename or the rewrite, for three reasons:
- They would void gates 2 and 6, which prove the rename is mechanical.
- Moving code in the same commit as renaming it can push a file below git's 50% similarity rename detection, and blame is lost.
- A structural mistake buried inside a history rewrite can't be bisected.

History needs no stripping: the pack is 12.6 MB.

| # | finding | fix |
|---|---|---|
| 1 | `cairn-cli` → `cairn-push`: one composition root depends on another, for `cairn_push::store::SqliteStore` (`commands/push.rs:208`) | move the store behind a port in infra (or a small shared crate); cli drops the push dep |
| 2 | `cairn-ffi-wasm` → `cairn-domain`: the dependency points inward, so it's legal. The CLAUDE.md table says "core" only | fix the table |
| 3 | `cairn-cloud` missing from the CLAUDE.md crate map | add the row |
| 4 | god files: `cairn-server/src/main.rs` 169 KB, `cairn-infra/src/transport.rs` 157 KB | split into modules with no behavior change; transport ships with before/after bench numbers (project rule) |
| 5 | 59 plans (16.6k lines) in `docs/plans`, plus stray `plans/`, `.zcode/plans/`, `docs/WEEK-01-PLAN.md` | delete executed plans (they stay in `cairn-archive` and git history); keep only the live ones |
| 6 | `archive/` (116 files), `tool/d5_field_leg.sh` | delete if nothing references them |
| 7 | 3 security docs (`SECURITY.md`, `docs/SECURITY.md`, `docs/SECURITY-MODEL.md`) | merge into one |
| 8 | 45 ADRs, no status index | keep them (they're the immutable record); add `docs/adr/README.md` with an accepted/superseded index |
| 9 | domain purity | clean: the one `tokio` hit in `predicate_compile.rs:83` is a comment |
| 10 | fresh docs | README, ARCHITECTURE with a hexagon diagram, QUICKSTART, one README per crate, `docs/ci/explainer.html`; docs-curator agent afterwards |
