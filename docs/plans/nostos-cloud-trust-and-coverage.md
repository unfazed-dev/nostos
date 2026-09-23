# Nostos Cloud — License Trust + E2E Coverage

**Started:** 2026-07-13. **Owner:** Claude (tech lead). **Status:** PLAN — no
implementation without explicit operator go (standing scope rule: plans only,
nostos tree only).

Companion to the ratified client-SDK redesign plan. That
plan covers the client SDK; this one covers the **server/cloud trust plane** the
client never sees.

## Why (verified 2026-07-13)

Two gaps surfaced when the operator asked "what about the API keys for the nostos
cloud business — all the smoke/e2e is self-hosted":

1. **License trust is env-only — not actually enforced.** STRATEGY §7.2 says
   *"licenses gate Nostos Cloud (managed hosting)… the managed `nostos-server`
   verifies a presented license by recomputing the HMAC."* Reality: `nostos-server`
   reads `NOSTOS_TIER` from env (`crates/nostos-server/src/main.rs:88`, default
   `enterprise`) and passes it to `SessionManager` (`main.rs:168-175`), which
   enforces `tier.device_cap()` (`crates/nostos-application/src/session.rs:60`).
   **No HMAC license is ever verified at runtime.** A self-host operator just
   sets the env. `LicenseClaims::sign`/`verify` exist
   (`crates/nostos-cloud/src/license.rs`, used at `routes.rs:543`) but only on the
   **minting** side (nostos-cloud). nostos-server doesn't consume them.
2. **The cloud business path has no integration/e2e coverage.** The SDK sweep
   (`scripts/sdk-e2e.sh`) is entirely `NOSTOS_SYNC_AUTH=none` self-hosted. nostos-cloud
   has ~31 unit/route tests (auth/license/routes/stripe/store) including a
   route-level signup→keys→license smoke (`routes.rs:548+`, in-process `oneshot`),
   but **nothing** ties: API-key mint → license mint → **server verifies +
   enforces tier** → Stripe webhook flips tier → cap changes.

The API key itself is **server→cloud** (`store.rs:64`: *"the sync SERVER presents
it to report usage + license"*) — never a client credential. Client→server stays
`none` | `supabase-jwt` (ADR-0010). So this plan changes zero client-side surface.

## Workstream A — wire offline license verification into nostos-server

**Problem:** `LicenseClaims` lives in `nostos-cloud`; `nostos-server` is a sibling
that must not depend on `nostos-cloud` (hexagonal independence — the license.rs
comment names this explicitly for `Tier`, which was already hoisted to
`nostos-domain`).

**Approach:** introduce a shared sibling crate **`nostos-license`** (depends only
on `nostos-domain` for `Tier`) holding `LicenseClaims` + `sign` + `verify` + the
`LicenseError` type — moved verbatim out of `nostos-cloud/src/license.rs`.
`nostos-cloud` and `nostos-server` both depend on `nostos-license`. (Rationale over
dumping it into `nostos-domain`: keeps the pure-types ring free of `hmac`/`sha2`/
`time` crypto deps; `nostos-license` is a small, named, reviewable trust seam.)

**nostos-server startup change** (`main.rs`, the tier-resolution block):
- Add args: `--license` (`NOSTOS_LICENSE`, the `<payload>.<sig>` string) and
  `--license-secret` (`NOSTOS_LICENSE_SECRET`).
- Resolution order:
  1. If `NOSTOS_LICENSE` + `NOSTOS_LICENSE_SECRET` are both present →
     `LicenseClaims::verify(token, secret)`; on success use the token's
     `tier`/`device_cap` (authoritative); on failure (bad sig / expired /
     malformed) → **bail** (a managed server must not silently fall back).
  2. Else → fall back to `NOSTOS_TIER` env (default `enterprise`), unchanged.
     OSS self-host stays free + unlimited.
- `SessionManager::new(store, tier)` receives the *verified* tier. Device-cap
  enforcement (`session.rs:60`) is already correct — it just finally trusts a
  signed credential instead of env.

`make ci` is the gate; `make bench` must not regress (no hot-path change — verify
runs once at startup).

## Workstream B — cloud e2e coverage (the `cloud` slice)

A new integration test (a `cloud` slice in `scripts/sdk-e2e.sh` AND/OR a
`crates/nostos-cloud/tests/` binary) exercising the full entitlement loop with
**no external services** (in-process nostos-cloud router via the existing
`test_app` helper + the Stripe webhook's test-secret path; a managed
`nostos-server` constructed in-process or as a short-lived binary):

1. signup → cookie → `POST /v1/projects` → `POST /v1/projects/:id/keys` (mint
   API key) → `GET /v1/projects/:id/license` (mint license) — reuses the
   `routes.rs:548+` smoke plumbing.
2. Start a managed nostos-server with `NOSTOS_LICENSE=<token>` +
   `NOSTOS_LICENSE_SECRET=<secret>` (no `NOSTOS_TIER`) → assert it logs + uses the
   **licensed** tier (not the enterprise default).
3. Connect clients up to the licensed `device_cap`; assert the next connect is
   rejected with `DeviceCapReached` (`session.rs:25`) — proving the **signed**
   cap is enforced.
4. Fire a signed Stripe webhook (`stripe::verify_webhook` test path) that flips
   the subscription tier → re-mint the license → server re-verifies → assert the
   device cap changed accordingly.
5. Negative: a tampered license (flip one sig hex) → server **bails** (refuses to
   start), proving no silent env fallback under managed mode.

This is the first test in the repo that ties client → managed server → cloud
billing, and it makes the open-core trust boundary (ADR-0006) auditable.

## Preserve (moat — do NOT regress)

- **OSS self-host stays free + full-featured + unlimited.** With no license
  presented, nostos-server defaults to `enterprise` exactly as today. The license
  path is opt-in for managed deploys only (ADR-0006: "open-vs-managed boundary is
  purely operational & compliance, never feature gates").
- Hexagonal dependency direction (new `nostos-license` sibling; no server→cloud
  coupling).
- Client→server auth unchanged (ADR-0010: `none` | `supabase-jwt`). The API key
  is never a client credential.

## Open sub-decisions (to ratify on go)

- `nostos-license` as a new crate vs. hoisting `LicenseClaims` into `nostos-domain`
  (accepting crypto deps in the pure ring). Recommendation: new crate.
- Whether the server re-verifies the license on a periodic refresh (expiry
  partway through a long run) vs. startup-only. Recommendation: startup-only for
  v1 (offline-license model; `expires_at` checked at verify); add a refresh hook
  later if managed ops needs it.

## Status (2026-07-13) — WS-A shipped; WS-B unit-level shipped; live-slice deferred

Operator go received (*"start with the easiest one like the API fix first"*).
Implemented + `make ci` green (fmt + clippy `-D warnings` + full suite).
Adversarial domain review: **APPROVE** (trust boundary + hexagonal edges
verified clean).

**WS-A — DONE.** New `nostos-license` sibling crate (depends on `nostos-domain` +
crypto only; no server→cloud edge). `license.rs` moved verbatim via `git mv`;
`base64url_*` promoted to `pub` (nostos-cloud `auth` reuses them). Added
`resolve_entitlement(token, secret, fallback_tier) -> Result<ResolvedEntitlement,
LicenseError>` — the pure trust fn. nostos-server `main.rs` calls it at startup:
absent license → `NOSTOS_TIER` fallback (OSS self-host stays
`enterprise`/unlimited); **presented-but-invalid → fatal** (`anyhow::bail!`, no
silent downgrade — ADR-0006 is now *enforced*, not just asserted).
`SessionManager` gained `device_cap: u64` + `with_device_cap` so a license's
negotiated cap overrides the tier default. Open sub-decisions RESOLVED: new
crate (not hoisted to domain); startup-only verify (no periodic refresh).

**WS-B — unit-level DONE; live-subprocess slice DEFERRED.** Trust boundary
covered by 6 `resolve_tests` (absent→fallback, valid→tier+cap, device_cap
override, tampered→fatal, wrong-secret→fatal, expired→fatal) +
`with_device_cap_enforces_explicit_override`. Cloud-mint→server-consume is
proven by shared `nostos-license` types (both sides call the identical
`LicenseClaims::sign`/`verify`). **Deferred:** the full managed-server loop
(live WS-connect cap rejection + Stripe-webhook tier flip) needs `main.rs`
extracted into `pub async fn run(cfg)` in a new `lib.rs` (nostos-server is
bin-only today) so a test can spawn it in-process — tracked as the next slice,
not faked.

**Review fixes applied (domain-guardian):** stale `tier.rs` doc (nostos-cloud →
nostos-license); stale Tier re-export comment; **`NOSTOS_LICENSE_SECRET` made
env-only** (removed the `--license-secret` clap flag — the signing secret must
never land on argv/`ps`; read via `std::env::var`). Deferred: gate
`base64url_*` behind a util module (low risk, single consumer).

**Pre-existing fmt drift (unrelated):** `cargo fmt --all` also reformatted 9
files that were not fmt-clean at HEAD (`nostos-ffi-wasm/*`, `nostos-infra/*`,
`nostos-application/{lib,ports}.rs`, `nostos-client/tests/e2e_live_replication.rs`)
— kept so the gate is green; split before committing WS-A if a clean diff is
wanted.

Uncommitted (commit only when asked).

## Explicit-go gate

Operator go received 2026-07-13 (above). Per standing scope: plans only; nostos
tree only; commit only when asked.
