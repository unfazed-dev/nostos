# v0.2.0 release handoff — everything after `git tag` is operator

- **State (2026-09-01):** tag `v0.2.0` cut locally at `05bb0d1`
  (changelog final, pubspec + hook/prebuilt.json aligned). Nothing pushed —
  pushing is the release act.
- **Why this tag matters:** the first tag ever carrying `sdk/nostos_flutter`,
  and the trigger for the prebuilts pipeline that makes consumers
  Rust-toolchain-free (arxa kit plan D3 0c).

## Operator steps, in order

1. **Push the tag** (from the nostos repo):

       git push origin main && git push origin v0.2.0

   That push runs `.github/workflows/release.yml` (actionlint-validated):
   CLI/server builds (macOS arm64+x86_64, glibc-pinned Linux, Windows), the
   flutter-glue native artifacts (macos-universal; android arm64-v8a /
   armeabi-v7a / x86_64; ios device-arm64 + simulator-arm64 + simulator-x64),
   and the `update-manifest` job that emits `release-prebuilt-manifest.json`
   as a release asset plus a PR filling `hook/prebuilt.json` with real URLs
   + sha256s.
2. **Merge the manifest PR** once CI is green (the PR exists because the
   manifest must be committed after artifact hashes are known).
3. **Publish to pub.dev** (explicit, human step):

       git pull                     # MUST come first — see the warning below
       cd sdk/nostos_flutter && flutter pub publish --dry-run && flutter pub publish

   > **`git pull` is load-bearing here.** Step 2 lands the real urls+sha256s
   > on `main`; publish reads your *local* tree. Publish without pulling and
   > 0.2.0 goes to pub.dev carrying the placeholder manifest — every consumer
   > silently falls back to `cargo build`, and pub.dev versions cannot be
   > unpublished, so the only remedy is 0.2.1. Gate on:
   >
   >     test "$(grep -c '"url": "https' sdk/nostos_flutter/hook/prebuilt.json)" = 7
   >
   > Note the publish is interactive on first upload of a package name (OAuth
   > + it sets the uploader), so it wants a real terminal.

4. **Flip the arxa kit pin** (arxa repo, `kit/nostos/pubspec.yaml`): the
   `nostos_flutter` git `ref:` moves from the `580796e1…` full SHA to
   `v0.2.0` — the one-line bump kit plan D1 reserved for exactly this
   moment. Do this ONLY after step 1 (the tag must resolve on GitHub) or
   arxa CI clones fail.

   > **CORRECTION (2026-09-01).** A git `ref:` defeats the point of this
   > release. The manifest PR (step 2) fills `hook/prebuilt.json` on `main`
   > *after* the tag, so the tag's own tree keeps the placeholder manifest —
   > verified: 0 of 7 urls filled at the tagged commit. An arxa pinned by git
   > ref therefore takes the `cargo build` fallback and stays
   > Rust-toolchain-bound, which is exactly what D3 0c was meant to end.
   > Pin arxa to the **pub.dev** version (`nostos_flutter: ^0.2.0`, published
   > in step 3 from a tree that has the merged manifest) — or, if the git ref
   > is required for another reason, move the tag onto the post-merge commit
   > first. Verify either way with
   > `grep -c '"url": "https' <resolved>/hook/prebuilt.json` == 7.
5. **Zero-toolchain consumers** arrive when arxa's kit rebuilds against the
   published prebuilts: no Rust, no cargo-ndk, no NDK for app builds
   (D3 0c closed). The interim Rust-prerequisite note in kit/nostos's README
   retires with it.

## Verification the pipeline should show

- The GitHub Release for `v0.2.0` carries: 3 CLI/server archives, 7 flutter
  native artifacts (+ the iOS `.xcframework.zip` convenience bundle), and
  `release-prebuilt-manifest.json`.
- The manifest PR's `hook/prebuilt.json` diff fills every `url`/`sha256` for
  the 7 keys and keeps `"version": "0.2.0"`.
- After the merge, a consumer `flutter build` with NO Rust toolchain
  installed succeeds by downloading a prebuilt (the hook's sha256-verified
  path), and `NOSTOS_FLUTTER_CARGO_FEATURES=iroh` builds still fall back to
  source (prebuilts never carry the off-default iroh feature — ADR-0041 D7).