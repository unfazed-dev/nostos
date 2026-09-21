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

## ~~BLOCKER 3~~ — RESOLVED 2026-09-21: Maven publishing is now configured

`sdk/nostos_kotlin/android/build.gradle.kts` had **no `maven-publish` plugin, no
`publishing` block, no signing config**. It now has all three plus a
`centralBundle` task.

There is no official Gradle plugin for the Central Publishing Portal
(central.sonatype.org/publish/publish-portal-gradle, checked 2026-09-21 — every
listed option is a community plugin), so the build stays on stock
`maven-publish` + `signing`: stage into a local Maven layout, zip it, hand the
operator one uploadable bundle. No third-party plugin enters the build.

**Verified, not just written.** `./gradlew centralBundle` produces a 30-file
zip in Central's expected layout: `.aar` (3.5 MB), `-sources.jar`,
`-javadoc.jar` and `.pom`, each with md5/sha1/sha256/sha512. Signing is skipped
unless `-PsigningInMemoryKey` is supplied, so `assembleRelease` and CI stay
green on a machine with no GPG.

**The group id is a `-P` override**, defaulting to `io.github.unfazed-dev` —
the namespace decision is NOT baked in, and the default is the one Sonatype
auto-verifies for a GitHub signup.

What is still operator-only: a Portal account, the namespace verification, a
GPG key, and the upload itself
(`POST https://central.sonatype.com/api/v1/publisher/upload`).

**Build environment note — RESOLVED 2026-09-21 (second pass).** The Android
toolchain drift is closed. `sdk/nostos_kotlin/android` and
`sdk/nostos_react_native/android` now run **Gradle 9.7.1 + AGP 9.4.1** with
`compileSdk`/`targetSdk` 36 and Java 17, verified by `centralBundle` and
`assembleRelease` respectively.

The earlier "JDK 17–21 required, `JAVA_HOME` must point at `openjdk@21`" note
is obsolete and was a consequence of the pin, not of Android: Gradle 8.9 tops
out at JDK 22. Gradle 9.7.1 runs on JDK 17–26, so the machine's default JDK 26
builds both modules with no `JAVA_HOME` at all. `openjdk@21` is now
unnecessary.

Two AGP 9 breaking changes had to be handled (docs read, not guessed):
- **Built-in Kotlin.** AGP 9 compiles Kotlin itself and *rejects*
  `org.jetbrains.kotlin.android` alongside the new DSL
  (developer.android.com/build/migrate-to-built-in-kotlin). The plugin
  declaration was removed from both modules; `kotlinOptions` went with it
  (`jvmTarget` now defaults to `compileOptions.targetCompatibility`).
- **Source sets.** Built-in Kotlin reads the `kotlin` source set only. The
  modules declared their generated UniFFI Kotlin under `java.srcDirs`, which
  produced a **green build shipping an empty `classes.jar`** — the `.aar` had
  the 11.8 MB `.so` and zero classes. Fixed to `kotlin.srcDirs`, and a
  `verifyAar` task now fails the build if `classes.jar` has no `.class`
  entries. `centralBundle` depends on it. (Post-fix: 101 classes for
  nostos-kotlin, 106 for nostos-react-native.)

**Not bumped, deliberately.** The Flutter Android projects
(`apps/atlet/flutter/android`, `sdk/nostos_flutter/`) pin AGP 9.0.1 /
Kotlin 2.3.20 **and** `android.newDsl=false` + `android.builtInKotlin=false` —
all four written by the Flutter template, which owns them. Raising AGP there
independently of the Flutter SDK is how `flutter build` breaks; they move when
Flutter moves. `sdk/nostos_capacitor` pins AGP 8.13.0 to match the Capacitor 8.5
app template, and its `classpath` is overridden by the host app's buildscript
anyway. Their Gradle *wrappers* were taken to 9.7.1.

## BLOCKER 4 — identity is still a placeholder

`docs/IDENTITY.md` and the `NOSTOS-IDENTITY-PENDING` grep token record this.
Unresolved and load-bearing for a first publish:

- **Legal entity** — currently the GitHub handle `unfazed-dev`.
- *Maven namespace is no longer gated on this.* Signing up for the Central
  Portal with a GitHub account auto-verifies `io.github.<username>` with no
  domain and no DNS record — so a Maven release can ship before a domain
  exists, under `io.github.unfazed-dev`, and move later.
- **Primary domain** — none registered; the repo URL stands in for `homepage`,
  which every registry displays.
- **Contact email** — none. `SECURITY.md` previously pointed at a mailbox on an
  unregistered domain; that was removed rather than left to swallow reports.

Maven Central's namespace verification and a public launch both want a real
domain and a real mailbox. This is the decision that gates the most.

## BLOCKER 5 — the `v0.2.0` tag is 112 commits stale

`v0.2.0` sat at `8e7b548` — 119 commits behind `HEAD` by the end of
2026-09-21, and the tag predates the whole tauri / web-multi-tab /
toolchain-bump run. **Re-cut locally on 2026-09-21** (see the closing note);
the old object is recoverable with `git tag -a v0.2.0 8e7b548`. It was never
pushed, so nothing downstream saw it.

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
5. ~~Re-cut `v0.2.0`~~ — done locally 2026-09-21. **Still un-pushed on
   purpose**: `git push origin v0.2.0` fires `release.yml`, which builds and
   publishes artifacts. Push it when you actually mean to release.
6. Merge the manifest PR the release workflow opens.
7. `git pull`, then `cd sdk/nostos_flutter && flutter pub publish --dry-run &&
   flutter pub publish` — gated on
   `test "$(grep -c '"url": "https' sdk/nostos_flutter/hook/prebuilt.json)" = 7`.
8. ~~Write the Kotlin `maven-publish` block~~ — done 2026-09-21 and verified
   end to end. What is left is the Portal account + GPG key + the upload.

Show HN timing and Nostos Cloud alpha stay where the roadmap put them: founder
calls, not engineering tasks. Drafts are in `docs/launch/`.

## Closing note — 2026-09-21, second pass

Done after the audit above was written:

- **The 100k cliff attribution was retracted.** `nostos-fanout-walk` measures the
  fan-out loop LINEAR from 10k to 100k sessions (0.458 → 0.486 µs/delivery) —
  ~1.4% of the per-event time the container run observed. It is not the cliff.
  The walk was parallelised anyway (2× with real sinks); the cliff is still
  open, with per-stage timing in the container as the next experiment. See
  `benches/results/RESULTS.md` § "The fan-out walk is linear".
- **BLOCKER 3 closed** — Kotlin `maven-publish` written and verified.
- **BLOCKER 4 partly defused** — `io.github.<github-username>` is auto-verified
  by a GitHub signup, so Maven no longer waits on a domain.
- **BLOCKER 5 closed locally** — `v0.2.0` re-cut on the current `main`. Old
  object was `8e7b548` (`git tag -a v0.2.0 8e7b548` restores it). **Deliberately
  not pushed.** Pushing the tag is the release trigger and that is your call.
- `openjdk@21` was installed via Homebrew during the first pass, then made
  unnecessary by the Gradle 9.7.1 bump (the second pass builds on the default
  JDK 26). Harmless to leave installed; nothing references it.

Untouched, still operator-only: crates.io naming (BLOCKER 1), every credential
(BLOCKER 2), the `@nostos-sync` npm org, identity (BLOCKER 4 proper), the tag push,
Show HN timing, Nostos Cloud alpha.

## What "shipping to Maven" actually means here

Maven Central is the default artifact repository for every JVM/Android build
tool — the thing `mavenCentral()` in a `repositories {}` block resolves
against. An Android consumer cannot write
`implementation("io.github.unfazed-dev:nostos-kotlin:0.2.0")` until that
coordinate exists there. It is the Kotlin/Android equivalent of npm for
`@nostos-sync/web` and crates.io for the Rust crates; Nostos needs all three because
it ships to all three ecosystems.

A coordinate is `groupId:artifactId:version` — here
`io.github.unfazed-dev` : `nostos-kotlin` : `0.2.0`. The group id must be a
namespace Sonatype has verified you control, which is why
`io.github.<github-username>` matters: a GitHub signup verifies it with no
domain and no DNS record.

What physically ships for one release is **six files plus checksums** —
exactly what `centralBundle` produces and what `verifyAar` now guards:

| file | why Central requires it |
|---|---|
| `nostos-kotlin-0.2.0.aar` | the library itself (classes + the arm64 `.so`) |
| `…-sources.jar` | required — source attachment for consumers' IDEs |
| `…-javadoc.jar` | required — generated here by Dokka via AGP |
| `…-0.2.0.pom` | name, description, url, licence, developers, scm — all required |
| `.asc` per file | GPG detached signature; the only piece this repo cannot produce |
| `.md5/.sha1/.sha256/.sha512` per file | integrity, written by Gradle |

The bundle is uploaded as one zip to
`POST https://central.sonatype.com/api/v1/publisher/upload` with a Portal
bearer token, then released from the Portal UI. **Releases are permanent** —
a published version can never be replaced, only superseded or deprecated.

So the remaining gap is not code. It is: a Portal account, a GPG key whose
public half is on a keyserver, and the decision to publish under
`io.github.unfazed-dev` versus waiting for a domain.
