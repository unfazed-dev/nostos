# v0.2.0 release — red-rail findings (2026-09-01)

Blocks the tag push. `main` was pushed (`c4f0071..071fe96`); the tag was NOT.

## What the handoff assumed vs what CI shows

The handoff recorded "rail green / sdk-e2e 7/7 host slices PASS" — that was
local. Remote CI on `main` has been red for the last 4 runs, three of them
timing out at `6h0m19s` (the GitHub Actions job cap, i.e. a hang).

Most recent completed run (33341541196) — 5 green, 2 red:

| job | result |
|---|---|
| real-Postgres logical-replication e2e | pass |
| fmt + clippy + test | pass |
| throughput benchmark (smoke) | pass |
| nostos_flutter — analyze + test | pass |
| nostos_react_native — typecheck | pass |
| **SDK live-replication e2e (host slices)** | **FAIL** |
| **cargo-deny (licenses, advisories, bans)** | **FAIL** |

## cargo-deny: advisories — FIXED by a lockfile bump

Three errors, all in the **default** (shipped) `nostos-server` tree — verified
with `cargo tree -p nostos-server -e normal -i <crate>`, so NOT confined to the
off-default `iroh` feature:

- `RUSTSEC-2026-0258` — h2 unbounded empty DATA frames (vulnerability), `h2 v0.4.15`
- yanked crate `chacha20 v0.10.1`
- yanked crate `spin v0.9.8`

`paste` / `atomic-polyfill` (unmaintained) are NOT in the default tree.

Fix, verified locally:

    cargo update -p h2 -p chacha20 -p spin@0.9.8
    # chacha20 0.10.1 -> 0.10.2 ; h2 0.4.15 -> 0.4.19 ; spin 0.9.8 -> 0.9.9

Result: `advisories ok, bans ok, sources ok`. Lockfile-only, 12 insertions /
12 deletions. Note `spin` needs the `@0.9.8` disambiguator — two versions are
in the lock, and a bare `-p spin` errors with "specification is ambiguous".

## cargo-deny: licenses — needs a POLICY decision (not a code fix)

Exactly one offender: **`ece@2.3.1`, MPL-2.0** — the RFC-8188 encrypted
content-encoding crate used by the Web Push path.

`deny.toml`'s allow-list has no `MPL-2.0`. MPL-2.0 is weak, file-level
copyleft: linking from an Apache-2.0 project is fine, obligations attach only
to modifications of the MPL files themselves. Two options — the user's call:

1. Add `"MPL-2.0"` to `deny.toml` `[licenses] allow`. One line.
2. Drop/replace `ece`, losing or reimplementing Web Push encryption.

## THE TRAP: local `cargo deny check` is NOT the CI gate

A first fix looked green locally and stayed red in CI. Reason:
`EmbarkStudios/cargo-deny-action@v2` runs with **`--all-features`**, so CI
resolves the whole graph including the off-default `iroh` tree. A bare local
`cargo deny check` resolves default features and cannot see it.

**Reproduce the CI gate exactly:**

    cargo deny --all-features check licenses advisories bans

Under `--all-features` two more classes fail, neither visible by default:

- **3 `Unlicense`-ONLY crates**: `ws_stream_wasm@0.7.5`, `pharos@0.5.3`,
  `async_io_stream@0.3.3` (the wasm websocket stack). Distinct from
  aho-corasick / byteorder / memchr, which are `MIT OR Unlicense` and resolve
  to MIT on their own — those never failed. Fixed by allowing `Unlicense`:
  a public-domain dedication, strictly more permissive than the already-allowed
  MIT, so it loosens nothing in spirit.
- **2 `unmaintained` advisories** (informational, not vulnerabilities):
  `atomic-polyfill@1.0.3` (RUSTSEC-2023-0089, superseded by portable-atomic,
  already in the graph) and `paste@1.0.15` (RUSTSEC-2024-0436, archived,
  feature-complete). Both transitive under `iroh`, neither fixable from this
  workspace. Ignored as **explicit IDs**, deliberately not by relaxing
  `unmaintained` wholesale, so a future unmaintained crate still fails.

Both shapes now report `advisories ok, bans ok, licenses ok`.

## release.yml could not have published anything

`release.yml` had **no `permissions:` block at all**, so it inherited the repo
default — verified `read`:

    gh api repos/unfazed-dev/nostos/actions/permissions/workflow
    {"default_workflow_permissions":"read","can_approve_pull_request_reviews":false}

With a read-only token, `create-release` (softprops/action-gh-release@v2)
cannot publish the Release, and `update-manifest`
(peter-evans/create-pull-request@v7) cannot open the manifest PR. The tag push
would have burned every build job and produced nothing.

Fixed with least privilege: top-level `contents: read`, `contents: write` on
`create-release`, `contents: write` + `pull-requests: write` on
`update-manifest`. `actionlint` clean.

**Still open — needs a repo setting, not YAML.** `can_approve_pull_request_reviews`
is `false`, which is the "Allow GitHub Actions to create and approve pull
requests" checkbox. While it is off, create-pull-request fails with *"GitHub
Actions is not permitted to create or approve pull requests"* no matter what
the YAML grants. Either enable it, or have the operator apply the manifest by
hand from the `release-prebuilt-manifest.json` release asset.

## SDK e2e — ROOT-CAUSED and fixed

`SDK live-replication e2e (host slices)` exits 3 = three failed slices
(dotnet, node, tauri; `rust` passed). All three died the same way, from the
uploaded slice logs:

    error: use of deprecated method
      `hmac::digest::generic_array::GenericArray::<T, N>::as_slice`
    error: could not compile `nostos-infra` (lib) due to 1 previous error

Not a test failure — a **deprecation promoted to an error** by the job-level
`RUSTFLAGS: -D warnings`. Two `digest` versions are in the lock (0.10.7 and
0.11.3); the newer one deprecates `as_slice` on `GenericArray`.

Sole call site, `crates/nostos-infra/src/auth.rs:137`. `self.digest` is
`[u8; 32]` (its `as_slice` is std and fine); the deprecated call was on the
`GenericArray` returned by `Sha256::digest`. Fixed by taking the same `.into()`
idiom line 127 already uses, then comparing arrays:

    let got: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    if got != self.digest {

Semantics unchanged (same fixed-length digest-vs-digest compare the doc
comment at line 106 describes). `cargo check --workspace --all-targets` under
`-D warnings` is clean; `cargo test -p nostos-infra --lib` = 207 passed,
including the three `static_bearer_*` tests covering this path.

## The 6h burns: uncapped jobs — FIXED

`fmt + clippy + test` and `throughput benchmark (smoke)` each burned
`6h0m16s` in runs 33285564940 and 33339998210, then passed in 3m56s / 4m43s
in 33341541196. `6h0m19s` is not a test duration — it is the GitHub Actions
default job cap.

Root cause of the *cost*: **not one workflow job had `timeout-minutes`.**
Verified `grep -c timeout-minutes .github/workflows/*.yml` == 0 across both
files. Any hang therefore ran until the 6h ceiling.

Fixed by capping all 15 jobs (ci.yml 7, release.yml 8), sized to the build
shape: 20 min for deny/typecheck/manifest/release, 30 for pg-e2e and flutter
analyze, 45 for lint-test / sdk-e2e / benchmark, 60 for the release
cross-compiles. Generous on purpose — a cap tighter than the real build turns
a working pipeline red.

**release.yml's caps had to land BEFORE the tag.** A `push: tags:` workflow
runs the file as it exists *at the pushed ref*, so an uncapped release.yml in
the tagged tree is unprotected no matter what `main` says later — the same
failure shape as the missing `permissions:` block above. Gate:

    git show v0.2.0:.github/workflows/release.yml | grep -c timeout-minutes   # == 8

Also added `concurrency: {group: ci-${{ github.ref }}, cancel-in-progress: true}`
to ci.yml. Three runs were in flight on `main` simultaneously on 2026-09-01
and starved each other. Deliberately NOT added to release.yml: cancelling a
half-finished release build would publish a partial artifact set.

## What the hang actually was — UNDIAGNOSED

An earlier revision of this document named
`fanout_scale.rs::ten_thousand_predicate_fanout_baseline` the "prime suspect"
for the 6h hangs. **That was wrong, and it is corrected here** so the guess is
not inherited as a finding.

That test measures elapsed wall-clock and asserts a floor. A slow machine makes
it *fail*, in about 30 seconds — it terminates by construction and cannot
produce a 6h hang. What was actually observed is two separate things:

- **A fast flake, real:** the floor is 50 zero-match events/sec at 10k
  predicates, asserted from a **debug** build. It failed locally at 38
  events/sec while another cargo job shared the CPU, and passed alone in
  48.94s. A wall-clock floor in a debug build on a shared runner will keep
  flapping. Worth moving to `--release`, gating behind the bench job, or
  widening — deliberately NOT done before the tag, since it changes what CI
  asserts and cannot help the release.
- **The hang, cause unknown.** No log survives the cap to say what stalled.
  Whether the JoinSet delivery path in this same test can also deadlock is
  unproven in both directions. The timeouts above bound the blast radius
  regardless of cause, and the next occurrence will now leave a readable log
  inside 45 minutes instead of a 6h truncation.

## Why the tag was held

`v0.2.0` is cut at `8e7b548` and has never been pushed, so re-cutting it costs
nothing right now. Pushing it would publish a server binary carrying a known
DoS advisory. The cheap ordering is: fix -> green -> move the tag -> push.

Once pushed, the ordering trap from the release handoff still applies: the
manifest PR fills `hook/prebuilt.json` on `main` AFTER the tag, so
`git pull` before `flutter pub publish` or 0.2.0 ships with the placeholder
manifest (verify with `grep -c '"url": "https' sdk/nostos_flutter/hook/prebuilt.json` == 7).

## Separate finding — the arxa kit pin (blocks handoff step 4)

Step 4 flips arxa's `kit/nostos/pubspec.yaml` to `ref: v0.2.0`, a **git** ref.
That tag's tree carries the placeholder manifest (verified: 0 of 7 urls filled
at `8e7b548`), so a git-ref consumer takes the cargo-build fallback and the
zero-Rust-toolchain promise — the headline of this release — does not hold.
Point arxa at the **pub.dev** version instead, or move the tag to include the
merged manifest commit.
