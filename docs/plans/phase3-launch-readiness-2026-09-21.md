# Phase 3 launch readiness — audit 2026-09-21

What an agent can finish, and what is genuinely an operator call. Every claim
below was checked against a live registry or the working tree on this date.

## Done this session

- `make ci` green (exit 0) on `0da3c8f`.
- `main` pushed: `edc2380..0da3c8f`, 38 commits. `origin/main` now matches local.
- React Native SDK verified: `tsc --noEmit` clean, jest 33/33 across
  `NostosClient` / `push` / `signout` / `watch`. It is healthy — only unpublished.
- All shipping package versions aligned to the `0.2.0` release train (they had
  drifted to `0.1.0`, with capacitor at `0.2.0-beta.1`).
- Bench container toolchain bumped `1.95 -> 1.98`: the workspace moved to
  rustc 1.98 in `b40bc65` and the bench image could no longer build it.

## BLOCKER 1 — crates.io name collisions

Three of the names this workspace would publish are already taken by unrelated
projects. Checked with `cargo search` on 2026-09-21:

| crate | crates.io status |
|---|---|
| `nostos` | **TAKEN** — 0.0.0, "Build-gated version control for Rust projects" |
| `nostos-core` | **TAKEN** — 0.1.0, "Core types, storage, and query engine for Nostos knowledge provenance" |
| `nostos-cli` | **TAKEN** — 0.1.6, "Backpac Agent-Native CLI for autonomous settlement workflows" |
| `nostos-domain`, `nostos-application`, `nostos-infra`, `nostos-server`, `nostos-client`, `nostos-ffi-wasm`, `nostos-license`, `nostos-push`, `nostos-bench` | free |

`nostos-core` and `nostos-cli` are both crates we ship. This needs a naming
decision before any `cargo publish`; renaming after publishing is not possible
(crates.io names are permanent, versions can only be yanked).

Options, cheapest first:
1. **Prefix only the two colliding crates** — `nostos-sync-core`, `nostos-sync-cli`.
   Smallest diff; leaves the other seven names as-is. Downside: inconsistent
   prefix across the published set.
2. **Prefix the whole set** — `nostos-sync-*`. Consistent, larger diff, and the
   binary name in `nostos-cli` (`nostos`) still collides with the `nostos` crate if
   anyone `cargo install`s both.
3. **Do not publish the library crates at all.** The SDKs are the product
   surface; the server ships as a binary/container. Publishing nine internal
   crates mostly serves docs.rs. This is the laziest option and worth
   considering seriously.

Not free on any registry, unrelated to the above: `nostos` on pub.dev,
`nostos_flutter` on pub.dev, and the `@nostos-sync` npm scope are all **free/empty**
(0 published packages), so those three rails have no naming problem.

## FIXED — two packaging bugs that would have shipped broken

Found by running `npm pack --dry-run` on each SDK, which nobody had done.

**`@nostos-sync/web` was publishing without its wasm core.** `wasm-pack` writes
`pkg-web/.gitignore` containing `*`, and npm falls back to a directory's
`.gitignore` when that directory has no `.npmignore`. The `files` allowlist in
`package.json` named `pkg-web/`, so it *looked* correct — but the tarball came
out as 11 files / 108 kB with the 269 kB `nostos_ffi_wasm_bg.wasm` silently
dropped. That package installs cleanly and fails at runtime.

Fixed by dropping an empty `.npmignore` into `pkg-web` from both `build:web` and
`prepack`. Tarball is now 16 files / 444.5 kB with the wasm present. Guarded by
`sdk/nostos_web/check-pack.cjs` (`npm run check:pack`) — verified to exit 1 and
name the missing files when the fix is removed.

**`@nostos-sync/node` had no `files` allowlist.** Its tarball carried `Cargo.toml`,
`Cargo.lock`, `build.rs`, 57 kB of `src/lib.rs`, and
`.claude-flow/data/pending-insights.jsonl` — a local agent-tooling artifact
leaking into a public package. It also ships one 8.5 MB `nostos_node.node` built
for the host arch with no platform gating, so a Linux or Windows install would
get a darwin-arm64 binary.

Marked `"private": true` rather than patched. The package's own description is
"napi-rs scaffold … feasibility proof, not a polished SDK", and proper napi
packaging means per-platform subpackages (`build:napi` already exists for it) —
that is real work, not a `files` field. **Veto this if you disagree**: it is a
one-line revert, and it is the only thing now preventing `@nostos-sync/node` from
being published.

`@nostos-sync/react-native` (20 files / 128.8 kB) and `@nostos-sync/capacitor`
(18 files / 91.1 kB) pack correctly.

## BLOCKER 2 — no publish credentials on this machine

| registry | state |
|---|---|
| npm | `npm whoami` -> `ENEEDAUTH`. Not logged in. |
| crates.io | no `~/.cargo/credentials.toml`. |
| Maven Central | no `~/.gradle/gradle.properties`, no `~/.m2/settings.xml`. |
| pub.dev | first upload of a package name is interactive OAuth; needs a real terminal. |

These are all human logins. An agent cannot create them, and should not.

## BLOCKER 3 — Maven publishing is not configured at all

`sdk/nostos_kotlin/android/build.gradle.kts` declares only `com.android.library`
and `kotlin("android")`. There is **no `maven-publish` plugin, no `publishing`
block, no signing config**. Maven Central additionally requires a verified
namespace (a domain you own, or `io.github.<handle>`) and a GPG key.

So "publish to Maven" is not a blocked command — it is unwritten build config,
gated in turn on BLOCKER 4.

## BLOCKER 4 — identity is still a placeholder

`docs/IDENTITY.md` and the `NOSTOS-IDENTITY-PENDING` grep token record this.
Unresolved and load-bearing for a first publish:

- **Legal entity** — currently the GitHub handle `unfazed-dev`.
- **Primary domain** — none registered; the repo URL stands in for `homepage`,
  which every registry displays.
- **Contact email** — none. `SECURITY.md` previously pointed at a mailbox on an
  unregistered domain; that was removed rather than left to swallow reports.

Maven Central's namespace verification and a public launch both want a real
domain and a real mailbox. This is the decision that gates the most.

## BLOCKER 5 — the `v0.2.0` tag is 112 commits stale

`v0.2.0` sits at `8e7b548`. `HEAD` is `0da3c8f`, **112 commits ahead**. The tag
predates the whole tauri / web-multi-tab / toolchain-bump run.

`.github/workflows/release.yml` fires on `push: tags: v*`, so pushing that tag
as-is would build release artifacts from a tree that is four months of work
behind and publish a prebuilt manifest to match. The release handoff's step 1
(`git push origin main && git push origin v0.2.0`) is **no longer safe as
written**.

Fix before releasing: delete the local tag and re-cut it on the pushed `main`
(`git tag -d v0.2.0 && git tag -a v0.2.0 -m ... 0da3c8f`). The handoff's own
2026-09-01 CORRECTION about the manifest PR landing after the tag still applies
on top of that.

## What the operator actually has to do, in order

1. **Decide the crate naming** (BLOCKER 1) — or decide not to publish crates.
2. **Decide identity** (BLOCKER 4): entity, domain, security mailbox.
3. Log in: `npm login`, `cargo login`, Sonatype account + GPG key.
4. Create the `@nostos-sync` npm org (the scope is empty, not necessarily claimable —
   only a logged-in attempt settles it).
5. Re-cut `v0.2.0` on `0da3c8f` (BLOCKER 5), then `git push origin v0.2.0`.
6. Merge the manifest PR the release workflow opens.
7. `git pull`, then `cd sdk/nostos_flutter && flutter pub publish --dry-run &&
   flutter pub publish` — gated on
   `test "$(grep -c '"url": "https' sdk/nostos_flutter/hook/prebuilt.json)" = 7`.
8. Write the Kotlin `maven-publish` block (BLOCKER 3) — a real task, ~40 lines,
   only worth doing once step 2 names the namespace.

Show HN timing and Nostos Cloud alpha stay where the roadmap put them: founder
calls, not engineering tasks. Drafts are in `docs/launch/`.
