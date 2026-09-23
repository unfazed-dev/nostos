# Launch Readiness Gap List — v0.1 → public launch → first customer

Date: 2026-07-12. Produced by full-repo sweep + smoke test (4 parallel audits:
docs claims rubric, code claim-verification, full smoke run, business readiness).
Method + evidence: every "shipped" claim in the master plan
(`complete-nostos-fully-wired-operational.md`, Phases A–F all `[x]`) was verified
against source with file:line evidence; smoke = `make ci` (≈273 tests, 0 fail),
WASM build, `reactive_scroll` e2e demo, real-Postgres e2e (97/97 with
`NOSTOS_E2E_PG=1`).

## Verdict

**v0.1 is code-complete and verifiably green.** All 13 doc-claimed features are
confirmed in source. Zero TODO/unimplemented markers, zero hexagonal violations,
domain + application layers panic-free. What remains is not engineering to
finish the project — it is (A) operator-gated launch operations, (B) a day of
pre-push doc/code fixes, and (C) a hardening list before the first paying
customer.

## A. Operator-gated launch blockers (Show HN) — Evan's calls

| # | Item | Notes |
|---|------|-------|
| A1 | Create GitHub repo + remote, push `main` + `v0.1.0` tag | `git remote -v` is empty; Cargo.toml metadata already points at `github.com/nostos-sync/nostos` |
| A2 | Fill `<repo>` placeholders in `docs/launch/show-hn-draft.md`, review both drafts, publish | Drafts are factually consistent with RESULTS.md (833k/208×, 10k drop rate honestly disclosed) |
| A3 | Show HN timing decision | Per ROADMAP.md footer: operator call |

> **Correction 2026-08-06:** row A2's "833k/208×" — the N× competitor-comparison framing compared fan-out to replication-ingest (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.

## B. Pre-push fixes (Claude-doable, ~1 day total, all Small)

| # | Item | Evidence |
|---|------|----------|
| B1 | README.md stale: headlines 142k/35.6× (current: 833k/208×), says "write-back v1 under way" (shipped), banner says "Phases 0–1 proven" (ROADMAP says Phase 3 🚧) | README.md:13,23,209 vs RESULTS.md:1-6; commit 96cbf2b missed README |
| B2 | Documented pg-e2e command omits `NOSTOS_E2E_PG=1` → e2e tests silently self-skip and report a **false-positive pass** | CLAUDE.md verbs section; e2e_pg_*.rs self-gate; CI sets the flag (ci.yml e2e-pg job) |
| B3 | `.env.example` missing `NOSTOS_TIER` (used at nostos-server/src/main.rs:88); no env example at all for nostos-cloud's 10 vars | main.rs:7-14 doc-comment only |
| B4 | Add SECURITY.md (sync engine + write-back trust boundary; HN audience) | none exists |
| B5 | ADR-0012 status line stale — still says wire/subscribe integration outstanding; Task C1 closed it | ADR-0012:3 vs plan C1 |
| B6 | STRATEGY.md Front-6 "three tiers" conflict-resolution claim needs shipped/deferred qualifier (only LWW shipped) | STRATEGY.md:122-125 vs ADR-0014 |

> **Correction 2026-08-06:** row B1's "142k/35.6×"/"833k/208×" — the N× competitor-comparison framing compared fan-out to replication-ingest (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.

## C. Hardening before first design partner / paying customer

| # | Item | Effort | Evidence |
|---|------|--------|----------|
| C1 | nostos-cloud deployment path: fly.toml or compose entry + hosting doc (Dockerfile exists, nothing stands it up) | M | Dockerfile:1-33; no fly.toml |
| C2 | Session cookie is unsigned bare account id — seal/sign before real accounts | S/M | routes.rs:131-138 (flagged in-code) |
| C3 | Password hashing is sha256 hex → argon2/bcrypt | S | store.rs:21-23 (flagged in-code) |
| C4 | `NOSTOS_CORS_ORIGINS` empty = fully permissive, no startup warning (unlike `NOSTOS_SYNC_AUTH=none`) | S | .env.example comment |
| C5 | nostos-server composition root has 0 tests over env→auth/tenant/write-back wiring (main.rs:160-310) | S/M | test inventory sweep |
| C6 | Snapshot buffers whole table in RAM — OOM ceiling on first big-table design partner | M | snapshot.rs:67,111 ponytail |
| C7 | pg.rs:617 invariant-by-convention `.expect()` on replication hot loop — make type-level | S | pg.rs:617 |
| C8 | Outbox failed writes retry forever, block queue head — dead-letter policy | M | client.rs:31,354; outbox.rs:47 ponytails |
| C9 | ToS + privacy policy stubs for the cloud product (none exist; no telemetry in code — good) | S | biz audit §7 |

## Grill decisions (2026-07-12, operator)

1. **Launch gate raised**: hold public launch until the Flutter+Supabase path is
   whole. Bar: Flutter dev → local-first offline Supabase todo app in **≤5 min**,
   plug-and-play, zero Rust visible, DX beats existing Supabase-integrated sync SDKs.
2. **Who runs the server**: the `nostos` CLI (prebuilt binary; `nostos init`
   auto-wires Supabase publication/config, `nostos dev` runs locally). CLI is
   free, Apache-2.0, no payment gate — license stays the moat.
3. **Revenue**: managed deploys (`nostos deploy`, hosted-lite, tier-stamped via
   existing HMAC/NOSTOS_TIER plumbing) → then full cloud. No FSL relicense.

## Implementation status (2026-07-12, swarm run)

W0a spike ✅ (native-assets path proven) · W1 ✅ (ADR-0018, e2e 105/105) ·
W2 ✅ (JWKS RS256/ES256/EdDSA + HS256 legacy, alg-confusion tested) · W3 ✅
(nostos CLI, 19 tests) · W4 ✅ (nostos_flutter, real-server integration test) ·
W5 ✅ (todo live mode; all 5 proof scenarios pass after the flush fix) · W6 ✅
(release.yml + brew + pub.dev dry-run; unverifiable-without-remote items listed
in workflow comments) · W7 ✅ · W8 ✅. **W0b ✅ VERIFIED (2026-07-12)** against
operator project `ltamqsxxumtusyxswezi` — see the W0b verdict below. Stranger
test (5-min stopwatch, fresh human+machine) is now the sole remaining
engineering-side launch gate; the rest are operator launch ops (§A).

Launch-blocking bug found & fixed by W5's proof: client txn batching buffered
forever on idle tables (ApplyEngine flush heuristic + outbox drain gated on
inbound traffic) — fixed via flush_quiesce (50ms) + write-notify, ADR-0016
addendum, new SyncClient+PgReplicator e2e suite (a combination previously
untested anywhere).

**New pre-launch DX item (F5):** PgReplicator renders every column value as a
JSON string ("done":"false", numbers as strings) — every typed consumer hits
this on first contact (W5 did). Typed payload mapping is deferred per
ADR-0016/0012; before launch either implement basic type mapping or document
the string-typing contract loudly in QUICKSTART + SDK README.

*F5 research verdict (2026-07-12, industry survey: pgoutput/Debezium/
ElectricSQL/Supabase-Realtime/wal2json/RFC 8259):* implement **server-side
OID-keyed mapping inside PgReplicator** — pgoutput Relation messages already
carry column type OIDs (`RelationMeta` currently discards them; pg.rs:89,842),
so no client schema artifact is needed (preserves the no-client-schema-artifact
differentiator; matches Supabase Realtime + current ElectricSQL direction).
Mapping: bool→bool; int2/4→number; float→number with NaN/±Inf→string guard
(RFC 8259 forbids them); numeric/decimal→string (arbitrary precision);
timestamps→RFC 3339 UTC strings; uuid/enum→string; bytea→base64;
json(b)→serialized string; arrays deferred. int8/oid/numeric/money→**string**,
settled 2026-07-12 by the non-CDC industry cross-check (protobuf JSON mapping,
Google Discovery, Twitter id_str, OpenAPI int64 format registry, GraphQL,
Stripe/Microsoft opaque-ID practice — unanimous: >2^53-capable ints are
strings on uncontrolled JSON wires; and dart2js `int` is 2^53-bounded, so
Flutter Web puts even our official client in the vulnerable population).
Predicate engine verified NOT at risk (it
already coerces text numerically; predicate.rs:538). No released clients exist,
so changing the wire NOW is free; after launch it's a breaking change.

*W0b verdict (2026-07-12, operator project `ltamqsxxumtusyxswezi`, live):* the
direct host is IPv6-only and this dev network has **no IPv6 egress** (router
assigns a global v6 address but doesn't route it; a corp full-tunnel VPN holds
the default route; no passwordless sudo). Resolved with a **userspace
Cloudflare WARP tunnel** — `scripts/warp-ipv6-egress.sh` (`wgcf` + `wireproxy`;
no sudo, no macOS system extension, doesn't disturb the existing VPN) exposes
`127.0.0.1:15433` → the Supabase IPv6 host. Through it, nostos's **real
`PgReplicator`** ran the full e2e against live Supabase PG 17.6
(wal_level=logical): `postgres` role creates/drops a pgoutput slot +
publication, 5 slots / 0 used, snapshot + live insert + LSN-resume all green
(`e2e_pg_replication` 3/3, `e2e_pg_snapshot` 2/2), identical to the local-PG
control run. nostos connects NoTls (plaintext); Supabase's direct host permits
it, so the tunnel URL uses `sslmode=disable`. Gotchas surfaced: (1) the pooler
cannot carry logical replication (confirmed — direct host required); (2)
adding TLS to the replicator is a C-tier hardening item (production
sync-server → Supabase should encrypt), not a dev-path launch blocker; (3)
wireproxy's `Address` must stay comma-separated on one line or v6 egress
silently fails. Project left clean (slots/tasks/pub all dropped post-test).
QUICKSTART documents the WARP fix.

## F. New blockers surfaced by the Supabase bar

| # | Item | Evidence |
|---|------|----------|
| F1 | **Write-back has no tenant enforcement** — allowlist-only gate; `Principal` never reaches the write path; authenticated user can write other tenants' rows (RLS is bypassed by the privileged PG connection) | transport.rs:392-405, write_back.rs (no principal refs) |
| F2 | Prebuilt native binaries for the Flutter package (no Rust toolchain on dev machines) — cross-compile + release pipeline | new |
| F3 | Verify current Supabase docs: logical replication needs the direct connection (pooler won't carry it); free-tier/IPv4 caveats affect the "5 min on free tier" promise | docs-first check at plan time |
| F4 | Client-side query/watch DX for Flutter (a comparable SDK's watch-SQL is the feature to match; `rows_for` is InMemory-only today) | in_memory.rs:59 |

## D. Known-deferred, fine to keep deferring (already honestly disclosed)

- 10k-client drop rate 61.4% → table-sharded router (Phase 2; disclosed in both launch drafts)
- OPFS browser durability (ADR-0017, post-v0.1)
- Flutter/RN/Node SDKs (Part VIII registry; Flutter first, gated on v0.1 tag)
- crates.io publish + release automation (metadata ready)
- Real send→recv latency metric (wire-v2)
- `rows_for` readback is InMemory/WASM-only; native SqliteStorage has no equivalent

## E. Path to first revenue (per Part VIII + STRATEGY.md)

1. Land A + B → public OSS launch.
2. C1–C4 + C9 → deployable cloud with live Stripe keys (checkout→webhook→license
   flow is real, tested code; needs keys + an instance + legal stubs).
3. Author `cloud-beta.md` (gated: v0.1 + first design partner) → Fly.io deploy,
   Stripe live mode, tier caps.
4. 5–10 design partners from the Show HN/Flutter-community wedge; their traffic
   gates `phase2-hardening.md` (C6/C8 + sharded router live there).
