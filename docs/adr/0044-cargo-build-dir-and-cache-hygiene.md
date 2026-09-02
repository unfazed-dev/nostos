# ADR-0044: Cargo intermediates live on the external SSD via a global `build-dir`; caches are swept weekly

**Status:** Accepted (2026-09-02). Operator-machine policy, not a code change.
Applies to every Rust workspace on this device (nostos, arxa, pixel_77 and
their SDK crates), not only nostos. Recorded here because nostos is the
heaviest Rust consumer and its SDK builds (cargo-ndk, native_toolchain_rust,
Tauri) are the ones most likely to break if the layout is wrong.

## Context

Rust builds were eating the internal disk. Measured on 2026-09-02:

- ~27.6 GB of `target/` directories across the three repos plus stale
  worktrees, all on the internal drive or on `business_ssd`.
- `~/.gradle` 4.6 GB and `~/.pub-cache` 3.3 GB as real directories in
  `$HOME`; `~/.cargo` and `~/.rustup` were already symlinks to
  `/Volumes/developer_ssd/dev/`.
- `~/.cargo/registry` 1.4 GB (external, fine).

The bulk is not the final binaries. It is `target/debug/{deps,build,
incremental,.fingerprint}` — per-dependency rlibs with full debuginfo, plus
incremental caches that Cargo never garbage-collects on its own. Every
`cargo test` in a fresh clone or worktree rebuilds the entire dependency tree
into a new `target/`, and old worktrees keep theirs forever.

Industry practice (Cargo docs, cargo-sweep, sccache/CI guides) converges on
three levers, in this order: (1) move intermediates out of the project tree
to one shared location, (2) drop debuginfo for dependencies in `dev`, and
(3) prune by age with `cargo-sweep`. Cargo 1.91+ added the stable
`build.build-dir` setting, which does (1) *without* moving the final
artifacts — bins, cdylibs and staticlibs are still uplifted into
`target/<triple>/<profile>/`, so downstream tools that read `target/` keep
working.

Verified before adopting (all on the pinned 1.95.0 toolchain, this machine):

- `build.build-dir` with `{workspace-path-hash}` splits intermediates to the
  external SSD and a cdylib still lands under `target/` — this is what
  cargo-ndk (`-o/--output-dir` reads `target/<triple>/`), Tauri's bundler and
  `native_toolchain_rust` (passes an explicit `--target-dir` per package;
  `build-dir` is orthogonal to it) depend on.
- `profile.dev.package."*".debug = false` from a config file merges over the
  workspace `Cargo.toml` profile with **no warning** on 1.95.0.
- Both `developer_ssd` and `business_ssd` are APFS (fingerprint mtimes and
  hardlink uplift behave; exFAT would not).

## Decision

1. **Global `build-dir` on the external SSD**, keyed per workspace:

   ```toml
   # $CARGO_HOME/config.toml  (~/.cargo -> /Volumes/developer_ssd/dev/.cargo)
   [build]
   build-dir = "/Volumes/developer_ssd/dev/cargo-build/{workspace-path-hash}"
   ```

   Per-workspace (not one shared dir) so rust-analyzer, `dx`, and parallel
   repos do not serialise on a single build lock, and so a sweep or `rm -rf`
   is scoped to one project. `target/` stays in each repo and holds only
   final artifacts.

2. **Dependencies build without debuginfo in `dev`:**

   ```toml
   [profile.dev.package."*"]
   debug = false
   ```

   Leaf crates (the ones you actually step through) keep full debuginfo.
   Backtraces into dependencies lose line numbers; that is the accepted
   trade. `incremental` is left at its default (on) — it is the thing that
   makes edit-compile loops fast and the sweep bounds its growth.

3. **Weekly mtime prune** of `/Volumes/developer_ssd/dev/cargo-build` by
   `~/Library/Scripts/cargo-build-sweep.sh` (`find -mindepth 3 -type f
   -mtime +7 -delete`, then drop empty dirs), triggered from `~/.zshrc` at
   most once per 7 days (stamp file in `~/Library/Logs`), logging to
   `~/Library/Logs/cargo-build-sweep.log`. Anything not touched in 7 days is
   rebuilt on demand: Cargo's fingerprint check verifies outputs exist, so
   a missing rlib or incremental dir is a rebuild, not a corrupt build.
   A launchd agent was tried first and rejected: launchd-spawned processes
   are denied removable-volume access by TCC (`find: Operation not
   permitted`) and exit 0 having swept nothing. The only launchd fix is
   Full Disk Access for `/bin/sh`, which is not acceptable; a terminal
   shell already holds the grant.
   `cargo-sweep` was tried first and rejected — 0.8.0 locates the directory
   to sweep via `cargo metadata`'s `target_directory`, which under this
   layout is the repo `target/` (final artifacts), not the build-dir; it has
   no way to see the intermediates at all.

4. **`~/.gradle` and `~/.pub-cache` become symlinks** to
   `/Volumes/developer_ssd/dev/{.gradle,.pub-cache}`, the same pattern
   already used for `~/.cargo` and `~/.rustup`, so Xcode-, Gradle- and
   Android-Studio-launched builds see the same caches as the shell without
   depending on environment variables. After the pub-cache move,
   `flutter pub get` is re-run in `sdk/nostos_flutter` because
   `.dart_tool/package_config.json` stores absolute pub-cache paths.

5. **Existing `target/` directories are deleted once**, after the config is
   in place, so the next build populates the new layout. Nothing in them is
   non-regenerable.

Not adopted: `sccache` (already disabled — it conflicts with `dx` setting
`RUSTC_WRAPPER`, see the existing comment in `config.toml`), a single shared
`CARGO_TARGET_DIR` (breaks cargo-ndk/Tauri artifact discovery and shares one
lock across every workspace), and `[profile.dev] opt-level` changes (not a
disk lever).

## Consequences

- Internal disk stops growing from Rust builds. Cost is that an unmounted
  `developer_ssd` makes every `cargo` invocation fail loudly (already true
  for the toolchain itself via `~/.rustup`).
- One full rebuild per workspace after the profile change.
- The sweep only runs when a terminal is opened; a week of GUI-only use
  delays it, which is harmless (it prunes more when it does run).
- `--time 7` means a week away from a project costs one cold rebuild on
  return. Acceptable; the registry cache (external) makes it a compile, not
  a download.
- The prune is by file mtime, not by fingerprint graph, so a partially
  pruned dependency is possible; Cargo treats that as "outputs missing →
  rebuild", verified in the plan. Hash dirs whose workspace no longer exists
  drain to nothing after 7 days on their own.

## References

- Cargo reference, `build.build-dir` (stable 1.91) and `{workspace-path-hash}`
  template — https://doc.rust-lang.org/cargo/reference/config.html#buildbuild-dir
- Cargo reference, profile overrides for dependencies —
  https://doc.rust-lang.org/cargo/reference/profiles.html#overrides
- cargo-sweep (evaluated, rejected) — https://github.com/holmgr/cargo-sweep
- Operator plan with the exact commands, measurements and rollback:
  `arxa/docs/plans/rust-target-dir-hygiene.md`
- ADR-0043 for the docs/plans convention this follows.
