# ADR-0006: Open-core, monetize via Cloud + Enterprise (the Postgres/Supabase play)

- **Status:** Accepted
- **Date:** 2026-06-26

## Context

We need a business model that (a) maximizes adoption (open) and (b) captures enough revenue to fund the company. Some competing sync engines monetize via metered Cloud plus a source-available self-host license. The strategy doc identifies the wedge: be the *clean-open* default and capture value through operations and trust.

## Decision

**Commoditize the engine; capture value through operations and trust** — the Postgres/Supabase model.

- **Self-hosted (Apache-2.0): 100% free, forever, full-featured, unlimited.** Not crippled open-core. This is the land.
- **Nostos Cloud (managed):** autoscaling, dashboard, observability, multi-region. The convenience premium.
- **Enterprise:** SSO/SAML, SOC2/HIPAA, SLA + indemnification, VPC/on-prem, dedicated tenancy.

**The open-vs-managed boundary is purely operational & compliance, never feature gates.** Everything functional is free in OSS.

### Pricing (transparent, predictable)

| Tier | Price | Notes |
|---|---|---|
| Hobby | Free | 1 GB synced/mo, 10k peak devices |
| Pro | $49/mo + overages | $0.50 / million sync ops, $0.10 / GB-mo stored |
| Scale | $499/mo + overages | volume discounts |
| Enterprise | Custom | SSO, compliance, SLA, on-prem |

## Rationale

- **Land-and-expand:** dev tries OSS locally (5-min setup) → ships to free Cloud → grows → Pro → Enterprise. No bait-and-switch.
- **Transparent pricing** is itself a wedge against per-op metering (the #1 cost complaint in this space).
- The Rust server's low footprint is a **Cloud margin advantage** — cost-to-serve is lower than a Node equivalent, so even cheap per-op pricing stays healthy.

## Consequences

**Positive:** maximal adoption funnel; clean upsell story; the Rust footprint helps Cloud margins.

**Negative:** giving away the engine forgoes license-based revenue; we're betting on Cloud/Enterprise scale. **Mitigation:** the Postgres/Supabase/Neon precedent shows this works at scale; and Apache-2.0 (ADR-0005) is what makes the adoption funnel wide enough to feed Cloud.

## Alternatives considered

- **Cripple open-core** (gate direct-write-back or CRDTs behind Enterprise). Rejected — creates the same metering resentment we're exploiting; shrinks the funnel.
- **Per-op metering.** Rejected — it's the #1 complaint about that model; we win on pricing trust by being predictable.
- **Pure SaaS (no self-host).** Rejected — forfeits the OSS-driven adoption wedge and the Realm-exodus market.
