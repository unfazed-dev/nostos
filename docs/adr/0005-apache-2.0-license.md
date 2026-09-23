# ADR-0005: Apache-2.0 license, end to end

- **Status:** Accepted
- **Date:** 2026-06-26

## Context

A common license choice among competing sync engines is **FSL** (Functional Source License): source-available, not OSI-open, 2-year change date, no-competing-use clause. This creates procurement friction for enterprise legal teams and a trust gap with OSS purists. The strategy (see `STRATEGY.md`) calls for a licensing wedge: be the *clean-open* default.

## Decision

**Apache-2.0** for every crate — server, core, and all SDKs. No FSL, no BSL, no "source-available" asterisk.

## Rationale

- **Procurement wedge:** enterprise legal approves Apache-2.0 in minutes; FSL/BSL trigger review. This is a real sales advantage.
- **Adoption wedge:** OSS purists and cloud providers will adopt and redistribute Apache-2.0 freely; FSL deters them. We win adoption, then capture value via Cloud (ADR-0006).
- **Moral high ground:** the cleanest possible "clean-open, no asterisks" story.
- **Patent grant:** Apache-2.0's explicit patent grant protects contributors and users (MIT doesn't).

## Consequences

**Positive:** maximal adoption; minimal legal friction; Apache-2.0 end to end is the headline differentiator.

**Negative:** a cloud provider *could* offer Nostos as a managed service without paying us. **Mitigation:**
1. Be the best operator of Nostos (Nostos Cloud) — the Supabase/Postgres model. Postgres is Apache-2.0-ish and Supabase/Neon/RDS built huge businesses operating it.
2. Move fast enough that a hyperscaler's "Nostos-as-a-service" lags the real thing.
3. Compete on operations + trust + Enterprise features (SSO, compliance, SLA) that hyperscalers do poorly for niche infra.
4. Trademark protection on "Nostos" / "Nostos Cloud" (license ≠ trademark).

## Alternatives considered

- **FSL/BSL** (HashiCorp-style model). Rejected — recreates the very procurement friction we're exploiting.
- **AGPL.** Rejected — network copyleft deters enterprise adoption; AWS-style forks have shown AGPL doesn't actually protect the way people think.
- **Elastic License 2.0.** Rejected — not OSI-open, community distrust, no conversion path.
