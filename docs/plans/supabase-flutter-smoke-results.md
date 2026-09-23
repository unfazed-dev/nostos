# Nostos × live Supabase — intensive smoke-test report

Date: 2026-07-12. Method: fable-mode five-gate loop. Operator project:
`ltamqsxxumtusyxswezi` (Supabase, PG 17.6, `wal_level=logical`). Network path:
userspace Cloudflare WARP tunnel (`scripts/warp-ipv6-egress.sh`) →
`127.0.0.1:15433` → Supabase IPv6-only direct host (free tier is IPv6-only;
this dev network has no IPv6 egress — the tunnel is the fix, see
`docs/plans/launch-readiness-gap-list.md` W0b verdict).

## Verdict

**The Nostos sync engine is proven against live Supabase: 18/18 automated e2e
tests green**, including concurrency exactly-once, LSN resume, typed payloads
(F5/ADR-0019), and the **full write-back tenant-enforcement suite (ADR-0018)
executed against live Supabase, not just local PG**. The Flutter SDK layer is
proven live visually (prior session) + at the unit level; the one unautomated
gap is a JWKS-auth Flutter integration run (blocked on a cloud user JWT, not on
code).

## Environment

- Supabase PG 17.6, `wal_level=logical`, `postgres` role `rolreplication=t`,
  5 slots / 0 used (verified clean post-run).
- nostos `PgReplicator` connects NoTls (plaintext); Supabase's direct host
  permits it, so the tunnel URL uses `sslmode=disable`.
- Tests run with `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgresql://…@127.0.0.1:15433/postgres`.

## Matrix — automated, live Supabase (18/18)

| Suite | Tests | Result | Key proof (observed) |
|---|---|---|---|
| `e2e_pg_replication` | 3 | ✅ | `pg_insert_reaches_ws_client`; fake-replicator WS delivery; **LSN resume: "3/3 missed events delivered on reconnect"** |
| `e2e_pg_snapshot` | 2 | ✅ | snapshot+stream; **concurrent writes: "42 concurrent rows, all appeared exactly once across 43 events"** |
| `e2e_pg_typed_payload` | 3 | ✅ | snapshot vs streamed byte-identical; **typed JSON renders correctly** (bool/int2/int4 native; int8/numeric/oid/money as String; F5/ADR-0019) |
| `e2e_pg_writeback` | 8 | ✅ | round-trip through replication; **tenant enforcement live**: cross-tenant delete rejected (row survives), cross-tenant insert stamped to caller's tenant, cross-tenant upsert conflict rejected, SQL-injection column name rejected, missing-row delete idempotent, own-tenant writes flow |
| `e2e_pg_sync` (client) | 2 | ✅ | idle-table flush-quiesce applies + advances checkpoint; mid-session write reaches Postgres without reconnect |

Commands:
```
NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgresql://postgres:<pw>@127.0.0.1:15433/postgres \
  cargo test -p nostos-infra --features pg \
    --test e2e_pg_replication --test e2e_pg_snapshot \
    --test e2e_pg_typed_payload --test e2e_pg_writeback -- --test-threads=1
NOSTOS_E2E_PG=1 ... cargo test -p nostos-client --features pg --test e2e_pg_sync -- --test-threads=1
```

## Scenario coverage (vs the planned matrix in `scratchpad/smoke-matrix.md`)

Covered (A = inbound, B/C = auth+write, D = edge):
- A1 initial snapshot · A2 live INSERT · A5 bulk/concurrent (exactly-once) ·
  A6 reconnect · A7 LSN resume · A8 idle-then-write quiesce
- B2/C write-back tenant enforcement (live) · C1 device→Postgres round-trip ·
  C3 optimistic local apply (engine layer)
- D1 typed payloads · D3 NULL round-trip (via typed) · D4 empty/idle ·
  D7 concurrent burst (exactly-once)

Flutter-SDK-specific layer:
- **Visual, live (prior session, 2026-07-12):** todo app showed a Supabase
  `INSERT` appear in the Flutter UI with a correctly-rendered unchecked `bool`
  (`todo-8-live.png`) — proves the full Flutter→nostos-server→Supabase inbound
  chain + F5 typed rendering on-screen.
- **Unit:** `sdk/nostos_flutter/test/nostos_test.dart` (FakeNostosEngine) covers
  subscribe/watch/write wiring + table-mismatch + JSON decode.
- The Flutter SDK wraps the engine above via FFI; that engine is the 18/18.

## Honest gaps (NOT claimed as verified)

- **Full JWKS-auth Flutter integration e2e:** unautomated. The macOS
  `integration_test` path (`nostos_live_test.dart`) needs `-d macos` packaging
  and a real Supabase-issued user JWT; only the project's anon key + DB password
  are in hand, not a cloud user credential. (Local HS256 auth is covered by
  `auth_sync.rs`; JWKS verification is unit-tested in W2.) **Blocker = creds,
  not code.**
- **Edge cases identified but not automated this pass:** ~1 MB payload column;
  mid-session `ALTER TABLE` schema change; auth-token expiry mid-session;
  explicit outbox dead-letter behavior (the outbox currently retries forever,
  head-of-queue blocking — ponytail debt, `client.rs:31-32`). These belong in
  the hardening list, not launch-gating.
- `nostos-server` composition root has 0 tests over the env→auth/tenant wiring
  (gap-list C5; unchanged).

## Cleanup

Post-run Supabase verified clean: 0 replication slots, 0 non-Supabase
publications, 0 public-schema tables. Test artifacts dropped: slots
`e2e_snap_*`, `e2e_client_sync_*`; publications `cairn_pub`,
`cairn_pub_typed_f5`; tables `tasks`, `typed_probe`.

## Bottom line

Nostos's core value proposition — Postgres logical replication → Rust fan-out →
on-device apply, with server-enforced tenant isolation and typed payloads — is
empirically solid against a real Supabase project. The remaining work is SDK-DX
parity, not engine correctness.
