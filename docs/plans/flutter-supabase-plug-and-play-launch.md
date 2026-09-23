# Flutter + Supabase Plug-and-Play — the v0.2 Launch Plan

Date: 2026-07-12. Supersedes the launch gate in
`complete-nostos-fully-wired-operational.md` Phase F (v0.1 remains tagged locally;
public launch now gates on this plan). Companion: `launch-readiness-gap-list.md`
(sweep evidence + grill decision log).

## Operator decisions this plan implements (grill, 2026-07-12)

1. **Launch bar:** a Flutter dev with a Supabase project goes from "sees Nostos"
   → fully local-first, offline-capable todo app in **≤5 minutes**. Plug and
   play: add dependency, connect Supabase project, publication/auth/config
   auto-wired. Zero Rust visible. Must be a measurably better experience than
   existing sync-for-Supabase integrations.
2. **Who runs the server:** the `nostos` CLI (prebuilt binary; free, Apache-2.0,
   no payment gate — the license stays the moat). `nostos init` auto-wires,
   `nostos dev` runs locally, production self-host = one-click template.
3. **Revenue:** managed deploys (`nostos deploy`, hosted-lite, tier-stamped via
   existing HMAC/`NOSTOS_TIER` plumbing) as fast-follow behind a launch-day
   waitlist. Full cloud tiers after. No FSL relicense — ever.

## The competitive delta we must deliver (research-verified, July 2026)

A comparable sync-for-Supabase integration today: two signups/two dashboards, SQL step,
dashboard-authored stream rules with a Validate→Deploy cycle, a hand-written
backend-connector class (~30-40 lines) with a documented synchronous-write
footgun, duplicated client-side Dart schema. Its guide claims 10–15 min for
the wiring alone; realistic custom-app time 45–60+ min. Its Flutter SDK is
HTTP-only; Flutter web is beta.

Nostos's story: **one control plane (your Supabase project + one CLI)**, no
connector class (auth auto-wired from the Supabase session), no second schema
artifact, predicates in Dart code (`where_sql`) instead of a server-deployed
DSL, WebSocket transport, 833k ops/s Rust server. "Your queries are your sync
rules."

## Research ground truth (docs fetched 2026-07-11; re-verify in W0)

- **Supabase JWT:** projects created since 2025-10-01 sign with **asymmetric
  keys (RS256 default)**; JWKS at
  `https://<ref>.supabase.co/auth/v1/.well-known/jwks.json` (edge-cached
  10 min). HS256 legacy secret exists only on old projects. **Nostos's
  HS256-only verifier fails against every new Supabase project** → W2.
- **Replication:** direct connection ONLY (pooler cannot carry logical
  replication). Free plan direct connection is **IPv6-only**; IPv4 add-on is
  Pro+. `postgres` role can CREATE PUBLICATION + create slots. Slot/walsender
  cap is **5 on Nano–Medium** compute, shared with Realtime/Pipelines/backups.
  Idle-instance WAL growth is a known footgun (comparable sync products' guides
  tell users to hand-tune `max_wal_size`/`max_slot_wal_keep_size`) — `nostos init`
  should handle/warn automatically.
- **Programmatic setup:** Management API runs SQL (`POST
  /v1/projects/{ref}/database/query`, beta) with PAT or OAuth; official
  "Connect Supabase" OAuth-app program exists for one-click integrations.
- **RLS:** logical replication streams unfiltered rows and privileged writes
  bypass RLS — Nostos's server-side predicates (reads) and W1 (writes) ARE the
  authorization layer; document this explicitly.
- **Packaging:** Dart build hooks ("native assets") are stable (Flutter ≥3.38 /
  Dart ≥3.10) and pub.dev-publishable; frb 2.12+ recommends its native-assets
  backend (`native_toolchain_rust`) — Cargokit upstream archived 2026-03.
  A comparable sync product's v2.0 (May 2026) release validated exactly this
  pattern at scale. **Rust keeps owning SQLite** (cross-isolate watch
  invalidation is why that product moved its connection pool into Rust). Flutter web: punt v1 (custom
  sqlite3.wasm approach later; do NOT reuse nostos-ffi-wasm via JS interop).

## Workstreams

### W0 — De-risk spike (LAUNCH GATE; do first, ~2-3 days)
Needs the operator-provided Supabase project (see Operator items).
- [ ] Empirically verify against a fresh real project: asymmetric JWT default +
      JWKS fetch; direct-connection reachability (IPv6 story from this
      network); publication + slot creation as `postgres`; slot count on free
      compute; WAL-growth settings. Record results in the plan.
- [ ] Prove the packaging path end-to-end with a hello-world: frb
      native-assets backend + `native_toolchain_rust`, prebuilt cdylib
      downloaded by `hook/build.dart` from a GitHub release, consumed by a
      stock Flutter app with zero extra steps on a machine without Rust.
- [ ] Write the one-page W4 fallback if native-assets blocks: frb's maintained
      Cargokit fork with `precompiled_binaries` (URL + signed pubkey), same
      release artifacts. Exit: fallback documented even if unused.
- [ ] Re-check the leading competitor's onboarding hasn't materially improved
      (competitor target moves).

### W1 — Write-back tenant enforcement (security; scoped to write path ONLY)
- [ ] ADR: extend ADR-0011's server-enforced tenant scoping to writes
      (addendum to ADR-0013). Design: when tenant enforcement is active,
      transport passes `Principal` to the write path; INSERT force-stamps the
      tenant column with the principal's tenant value; UPDATE/DELETE add
      `AND <tenant_col> = <principal>` so cross-tenant rows are untouchable.
      Anonymous-auth mode (no tenant col) unchanged.
- [ ] Implement in transport + `PgWriteBack`; wire `Principal` through
      `handle_client_message` (transport.rs:373) — allowlist gate stays first.
- [ ] Chaos/e2e test: two principals, cross-tenant write attempts rejected,
      own-tenant writes flow. Extend `e2e_pg_writeback.rs`.

### W2 — Supabase JWT modernization (JWKS / RS256+ES256)
- [ ] Verifier: fetch + cache JWKS (≤10 min TTL, honor `kid`), RS256 + ES256,
      keep HS256 shared-secret as explicit legacy config
      (`NOSTOS_SYNC_AUTH=supabase-jwt` gains `NOSTOS_SUPABASE_JWKS_URL` /
      auto-derive from project URL; `NOSTOS_SUPABASE_JWT_SECRET` = legacy path).
- [ ] Key rotation: refetch on unknown `kid`; fail closed.
- [ ] Tests incl. fixture JWKS + rotated-key case; e2e against the W0 project.

### W3 — The `nostos` CLI
Single static binary (clap; new crate `nostos-cli`, or subcommands on
nostos-server — decide at implementation; separate crate keeps server lean).
- [ ] `nostos init`: prompt for direct connection string (v1; OAuth "Connect
      Supabase" app is a fast-follow), verify connectivity + `wal_level`,
      create publication + slot, count free slots and warn at the 5-slot cap,
      set/advise WAL retention (`max_slot_wal_keep_size`), detect JWKS URL,
      write `nostos.toml` (tables to sync, write allowlist, tenant column,
      auth). Idempotent re-runs.
- [ ] `nostos dev`: run the embedded/co-installed nostos-server from
      `nostos.toml`; print the ws:// URL + a copy-paste Flutter snippet.
- [ ] `nostos doctor`: connectivity, slot pressure, WAL lag, JWKS reachability.
- [ ] `nostos deploy --template fly|railway`: v1 = generate config + docs
      (self-host); managed mode joins waitlist (W8).
- [ ] Distribution: GitHub Releases (macOS arm64/x64, Linux x64/arm64,
      Windows x64) + `brew tap` + curl installer.

### W4 — `nostos_flutter` SDK (single-point-of-failure risk; W0 de-risks)
- [ ] Architecture: Rust owns SQLite (`nostos-client` as-is); frb
      native-assets backend exposes: `Nostos.connect(supabaseUrl|wsUrl,
      sessionToken)`, `subscribe(table, whereSql)` → typed row `Stream`s,
      `watch(table)` reactive query stream (Rust emits change notifications —
      it applied the changes), `write(...)` → durable outbox, offline/reconnect
      transparent.
- [ ] Supabase auth glue: accept a `supabase_flutter` session, auto-refresh
      token pass-through. No connector class, no client schema artifact.
- [ ] Platforms v1: iOS + Android + macOS (dev). Windows/Linux fast-follow;
      web punted (documented).
- [ ] Example + integration test on a real device/simulator in CI where
      feasible; otherwise scripted local verification documented.

### W5 — Supabase todo showcase (the acceptance test)
- [ ] Convert `fixtures/flutter/todo` live mode to Nostos: Supabase auth +
      nostos_flutter sync; offline create/complete/delete; two-user tenant
      isolation demo (proves W1/W2 visibly).
- [ ] The **5-minute quickstart** doc IS the acceptance script: `brew install
      nostos && nostos init && nostos dev` + `flutter pub add nostos_flutter` +
      ~10 lines of Dart. Stranger-test it (fresh machine, stopwatch, no author
      present) — same F1 methodology as v0.1.
- [ ] Exit: stranger hits working offline sync in ≤5:00 wall-clock.

### W6 — Release engineering
- [ ] GH Actions matrix: Android (cargo-ndk: arm64-v8a, armeabi-v7a, x86_64),
      iOS (XCFramework: device + sims), macOS (universal), Windows x64, Linux
      x64 (zigbuild, glibc-pinned) → GitHub Releases, content-hash versioned;
      `hook/build.dart` validates hash.
- [ ] CLI binaries in the same release; pub.dev publish dry-run; brew formula.
- [ ] Keep `make ci` green throughout; new crates join the workspace gates.

### W7 — Docs, bench, and pre-push hygiene (B-list from gap list)
- [ ] README rewrite: new headline numbers (833k fan-out, honest-units framing), Flutter+Supabase
      quickstart front and center, status banner current.
- [ ] Fix documented pg-e2e command everywhere to include `NOSTOS_E2E_PG=1`
      (CLAUDE.md, docs) — currently a silent false-pass.
- [ ] `.env.example`: add `NOSTOS_TIER`; create nostos-cloud env example.
      SECURITY.md. ADR-0012 status line. STRATEGY.md Front-6 qualifier.
- [ ] Launch drafts: add the Flutter+Supabase story + 5-min claim (only after
      W5's stranger test proves it); re-run the 1k benchmark near launch day so
      the headline is fresh (append, don't rewrite — operator call on timing).
- [ ] RLS-bypass documentation page: why Nostos predicates are the authz layer.

### W8 — Managed-deploy waitlist (launch day, minimal)
- [ ] Landing/README section + waitlist capture (nostos-cloud already has a
      waitlist table — reuse). No infra build pre-launch.

## Status (2026-07-12 swarm implementation)

All workstreams W0a–W8 implemented and verified locally — see
`launch-readiness-gap-list.md` "Implementation status" for the per-workstream
record, including the launch-blocking client flush bug W5's proof caught
(fixed; ADR-0016 addendum; new `e2e_pg_sync.rs` coverage). Remaining launch
gates, in order:

1. **W0b** — operator provides a fresh Supabase project; run the empirical
   verification + the ⏳-marked QUICKSTART steps + live JWKS/TLS paths
   (nostos-cli TLS heuristic and HTTPS JWKS fetch are code-complete but
   unverified against real Supabase).
2. **F5 typed payloads** — PgReplicator delivers all values as JSON strings;
   decide map-types-now vs document-loudly (see gap list F5).
3. **Stranger test** — fresh human + machine, stopwatch ≤5:00, using
   docs/QUICKSTART.md verbatim (warm-cache dry-run passed comfortably; cold
   cache depends on W6's prebuilt binaries being live).
4. **Operator launch ops** — GitHub org/repo + push, tag v0.2.0 (fires
   release.yml, fills prebuilt manifests), pub.dev publish, brew tap,
   benchmark re-run, drafts review, Show HN.

## Sequencing

```
W0 spike (gate) ──► W1 + W2 (parallel, engine) ──► W5 showcase ──► stranger test
                └─► W3 CLI ──────────────┬────────►
                └─► W4 SDK (post-spike) ──┘
W6 rides alongside W3/W4; W7 anytime after W1/W2 land; W8 is launch-day.
```
Launch = W0–W8 done + stranger test ≤5 min + operator publishes (repo push,
pub.dev, brew, drafts).

## Operator action items (blocking, in order of need)

1. **Now (blocks W0):** create a fresh free-tier Supabase project for
   testing; provide URL + direct connection string + a PAT (or just the DB
   password) — goes in untracked `.env` / `fixtures/flutter/todo/env.json`.
2. **Before W6:** push `main` to the existing repo —
   **`github.com/unfazed-dev/nostos`** (already the `origin` remote), needed for
   release artifacts + `hook/build.dart` URLs. pub.dev publisher account;
   brew tap repo (`unfazed-dev/homebrew-tap`).

   > Corrected 2026-07-30: this step used to read "create the GitHub org/repo
   > (`nostos-sync/nostos` per Cargo.toml)". **No such org exists or was ever
   > intended** — the name was invented, written into `Cargo.toml`, and then
   > cited back as if the manifest were evidence for it. It spread to 24 files
   > including the Homebrew formula's download URLs and the landing page's
   > "Star on GitHub" link. All corrected to `unfazed-dev`. Do not re-derive a
   > fact from a file that got the fact from you.
3. **Launch day:** benchmark re-run sign-off, drafts review, Show HN timing.
4. **Post-launch (W8→cloud beta):** Fly/hosting account, Stripe live keys,
   ToS/privacy (C-list in gap list).

## Risks (ranked)

1. **W4 toolchain immaturity** (frb native-assets is beta-versioned) —
   mitigated by W0 spike + written Cargokit-fork fallback.
2. **IPv6-only free-tier direct connection** — dev machines without IPv6 can't
   reach their own DB. W0 measures reality; mitigation options: document
   clearly, detect in `nostos doctor`, recommend Pro/IPv4 add-on, or a relay as
   managed-deploy value-add (revenue angle, not charity).
3. **Slot exhaustion on small computes (5)** — `nostos init` counts and warns;
   `nostos doctor` monitors.
4. **Supabase Pipelines** (first-party CDC, private alpha) — roadmap risk on
   the wedge; speed matters, ship before it broadens.
5. **Benchmark staleness** — headline ages while W0–W6 run; re-run pre-launch.
6. **Snapshot RAM ceiling** (gap list C6) — a design partner with a big table
   OOMs; keep in first-hardening slot after launch.

## Docs consulted

Supabase: setup-replication-external, manual-replication-faq,
connecting-to-postgres, ipv4-address, compute-add-ons, auth/signing-keys,
auth/jwts, row-level-security, build-a-supabase-oauth-integration, Management
API reference. Flutter/Dart: docs.flutter.dev bind-native-code,
dart.dev/tools/hooks, flutter_rust_bridge manual (cargokit, native-assets,
precompiled), native_toolchain_rust, cargokit precompiled_binaries.md, drift
web/streams docs. Full URLs in session research reports.
