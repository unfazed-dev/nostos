# ADR-0007: Platform assembly — Supabase + Rust + Cloudflare + Fly.io

Date: 2026-06-26
Status: Accepted

## Context

Nostos ships as two surfaces: an open-source **sync engine** (the Apache-2.0 binary
that streams Postgres → devices) and a **managed Cloud** control plane (accounts,
projects, billing, licenses). The founder asked: where does each piece run, and how
do they stay decoupled so day-1 works end-to-end without locking us into a vendor?

Constraints:
- The sync engine needs long-lived WebSocket connections — it can't run on
  serverless (Cloudflare Workers, Lambda) which kills idle connections.
- Auth must work for both **self-hosted OSS** users (no Supabase) and **managed
  Cloud** users (Supabase-backed identity).
- Billing is Stripe day-1; PayPal is desired but not launch-blocking.
- Push notifications (FCM/APNs) and reactive-by-default are roadmap, not day-1.
- The web surface (landing + admin) is already a SvelteKit static build.

## Decision

Split by **workload characteristics**, not by vendor preference:

| Workload | Host | Why |
|---|---|---|
| Static web (landing + admin) | **Cloudflare Pages** | SvelteKit `adapter-static` output. Cheapest, fastest, zero cold-start. |
| Identity + billing tables | **Supabase** (Postgres + Auth) | Managed auth, RLS, JWT issuance. Owns identity/billing ONLY — never runtime sync state. |
| Sync engine (`nostos-server`) | **Fly.io** | Long-lived WebSocket fan-out. Serverless can't hold connections. |
| Control plane (`nostos-cloud`) | **Fly.io** | Same reason; co-located with the engine. Stateless, horizontally scalable. |
| Operational Postgres (the source of truth being replicated) | Supabase / customer DB | The engine replicates from *any* Postgres via logical replication. |

### Auth: ADD, don't replace

`nostos-cloud` accepts **both** credential paths on every authed route:

1. `Authorization: Bearer <jwt>` — verified against the Supabase JWT secret
   (HS256). Managed-Cloud clients send this.
2. `cairn_session` cookie — the existing email/password → cookie flow. The
   self-hosted OSS path + the web admin use this.

The verifier is trait-abstracted (`JwtVerifier`) so `cfg(test)` injects one that
skips signature checks — smoke tests run with no external Supabase. This preserves
the OSS path (no Supabase dependency) while unlocking managed-cloud JWT auth.

### Billing: trait seam, Stripe live, PayPal stub

A `PaymentProvider` trait (`payments.rs`) with one method: `create_checkout`.
`StripeProvider` wraps the existing hand-rolled Stripe REST integration.
`PaypalProvider` is a stub behind the `paypal` feature flag that returns
`Unsupported` until wired. Day-1 ships Stripe-only; the seam is reserved.

### Reactive-default: server-side cadence, single-sourced

Per the reactive-default ultrathink decision: Nostos is **reactive-when-connected,
queue-when-offline** — the data contract is always reactive, but instant push is
not the default. The push cadence lives in **`FanOutService::push_interval`**
(server-side, single-source) so the four FFI bridges stay dumb. Default is
`Duration::ZERO` (instant, what the benchmark measures); a managed Cloud deploy
sets ~1-2s to coalesce bursts.

### Concurrent-device cap: enforced in the engine, tier from domain

`SessionManager::connect` enforces a peak concurrent-session limit derived from
`Tier::device_cap()` (now in `nostos-domain`, so both `nostos-server` and
`nostos-cloud` share one taxonomy without coupling). OSS self-host defaults to
Enterprise (unlimited); a managed deploy stamps the licensed tier via
`NOSTOS_TIER`. Caps are on **concurrency**, not registered-device count.

### Deferred (behind seams, not built)

- **PayPal** — stub compiles; real integration post-launch.
- **Push notifications** (FCM/APNs) — no day-1 surface; a `PushSink` port is the
  documented extension point.
- **Supabase RLS on billing tables** — the schema is designed for it; enabling is
  a config step, not a code change.

## Consequences

- **Two Rust binaries on Fly.io** (`nostos-server`, `nostos-cloud`), one static
  site on Cloudflare, one Supabase project for identity/billing. Four moving
  parts, each doing the one job it's best at.
- The OSS self-host path (`cargo run` + a Postgres) works with **zero** of the
  managed infra — no Supabase, no Cloudflare, no Fly.io. This is non-negotiable
  for the open-core trust model (ADR-0006).
- Auth has two paths forever. The trait abstraction keeps the divergence honest;
  the session-cookie path can be removed only if the founder decides OSS users
  must also use Supabase (not day-1).
- The `paypal` feature must be compiled in CI or the stub rots.
