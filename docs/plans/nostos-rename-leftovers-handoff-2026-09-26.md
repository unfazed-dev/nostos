# Handoff — cairn → nostos rename leftovers (2026-09-26)

State at handoff: `unfazed-dev/cairn-archive` archived; nostos `main` @ 8532702
(PRs #52–#56 merged); no open PRs; no worktrees; branch protection has 10
required contexts incl. `PR title stage tag`; local clone moved to
`…/cairn-archive`, session cwd is `…/nostos`.

Sources read: `docs/plans/nostos-rename-research-2026-09-24.md` §8,
`docs/ci/decisions.md` rows 4–14, `docs/OPERATING.md`, `git grep -i cairn`
(74 tracked files, 57 of them intentional — see "Leave alone").

## Leftovers

| # | item | owner | fix | PR tag |
|---|---|---|---|---|
| 1 | 10 `LICENSE` files plus the cloud landing footer say `Cairn Sync, Inc.` (the other five matches are historical docs or rename tooling) | user decides the legal name | update the 11 live copyright lines once the exact legal holder is known | `[arxa-cicd]` |
| 2 | `docs/plans` 47 files + stray top-level `plans/` (§8 #5) | agent, after user picks the keep list | delete executed plans (they survive in `cairn-archive` + history); keep live ones + this file | `[arxa-cicd]` |
| 3 | local `.nostos/config.json` project `cairn-app`; `.nostos/direct.sql` (75 hits, header `cairn link`); `.env` 5 stale comment lines | agent | `nostos link --mode direct` regenerates both `.nostos/*`; sed the `.env` comments (`CAIRN_`→`NOSTOS_`, `cairn_pub`→`nostos_pub`, plan path). All gitignored — no PR | — |
| 4 | `apps/atlet/supabase/migrations/0009_order_vendor.sql` guards on `to_regclass('cairn.push_templates')`; fresh DBs have `nostos.push_templates` from day one, so the icon-title update silently no-ops | agent | forward migration `0013_*.sql` (0011 and 0012 already exist) re-running the update against `nostos.push_templates` (guarded); apply to `exvjzhdrbcrnzosqvccv` after review | `[arxa-builder]` |
| 5 | `docs/ci/decisions.md` rows 4, 6–14 still headed "Defaulted while the user was away — review" | user reviews; agent edits | confirm or change each; move the header to "Reviewed <date>" | `[arxa-cicd]` |
| 6 | registries (decision 9): npm `@nostos-sync`, pub `nostos_flutter` unclaimed | user (needs npm/pub login) | create the npm organization while signed in; a first public pub.dev publish claims the package name, so review the release before publishing | — |

## Progress after handoff

- #3: regenerated the ignored local config and SQL; corrected five `.env`
  comment lines. The generated SQL's two remaining `cairn` mentions are
  intentional migration references.
- #4: migration 0013 is in PR #57. The live `order_events` title already has
  the icon; a rolled-back transaction verified the migration repairs a missing
  prefix and is idempotent. `make ci` passed.
- #2: the adopter playbook moved to `docs/guides/`. The qairn rename inventory
  and sync-scoping research are still cited by other plans, so they remain;
  a filename-reference scan found 42 of 46 preexisting plans cited elsewhere.
  The four uncited plans are a draft Atlet wave, unresolved code audit,
  pending field leg, and AI roadmap. Broader pruning awaits the keep-list
  decision and reference cleanup.
- #6: pub.dev has no `nostos_flutter` package yet; the `0.2.0` dry run passes
  with `--ignore-warnings`. Its three warnings cover two intentional exact FRB
  version pins and two tracked files ignored by Git.

## Leave alone (intentional cairn mentions)

- `rename:hold` fallbacks until 1.0 (decision 10, ADR-0046/0048): `CAIRN_*`
  env, `cairn.toml`/`.cairn`/`cairn_rules.toml`, `cairn-*.db`, binary symlinks
  (`packaging/legacy-binary-names.sh`), SQLite table/file migrate-in-place,
  `direct.rs` schema rename, Android channel delete, `CAIRN_PUSH_SECRET`.
- Brand metaphor "a cairn is a stack of stones": `docs/STRATEGY.md`, both
  `tokens.css`, `web/src/routes/+page.svelte`, `NostosField.svelte`,
  `web/_design/0-the-nostos-field.html`, cloud `tokens.css`.
- `scripts/nostos_rename/*`, `docs/retro/*`, ADRs, CHANGELOGs, migrations
  0002/0005/0010, `docs/OPERATING.md` upgrade note.

## Done since the audit (do not redo)

§8 #1 (cli→push dep gone), #2/#3 (CLAUDE.md crate map), #4 (god files split),
#6 (`archive/`, `tool/d5_field_leg.sh` deleted), #7 (one `SECURITY.md`),
#8 (`docs/adr/README.md`), #10 (QUICKSTART, 12/12 crate READMEs).

## Loop for each PR

`make worktree NAME=<task>` → edit → `make ci` → commit (single line,
conventional prefix) → push → `gh pr create` with the stage tag → user merges
on GitHub → `git pull --ff-only` → `make worktree-rm NAME=<task>`.
