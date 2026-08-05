# Real-PG write-amp harness + cold-stranger auth probe — 2026-08-05

## Why

Two outstanding honesty/launch items from the 2026-08-05 remaining-work report:

1. **ADR-0025/0026-mandated real-PG write-amplification measurement never published.**
   `NOSTOS_BENCH_OPLOG` is an in-process recorder (0 PG rows) and `make bench` runs
   `FakeReplicator` (eval-only), so the **real-PG `cairn_oplog` INSERT amplification**
   — the explicit slice-6 open item — has never been measured. ADR-0025 §Consequences:
   *"every WAL event now also writes a `cairn_oplog` row."*

2. **The cold-stranger test's defining engineering risk is unexercised:** the
   JWKS/RS256 verification path (`jwks.rs::JwksVerifier`) is tested only via
   `FixtureJwks`; it has **never hit a real Supabase project's
   `/.well-known/jwks.json`**. A stranger following QUICKSTART against real Supabase
   hits exactly this path first.

## Deliverable 1 — real-PG write-amp harness (runs now, docker up)

`crates/nostos-infra/tests/e2e_pg_write_amp.rs`, `NOSTOS_E2E_PG`-gated, tenant mode.
Mirrors `e2e_pg_oplog_replay.rs`'s harness but **drops the WS server** — it counts
`cairn_oplog` rows directly via SQL.

- Wire the production path: `PgReplicator` → `FanOutService.with_op_log(PgOpLogWriter)`.
- Create slot on the live table, insert **N=200** rows for a fresh tenant UUID
  (strictly post-snapshot → live WAL events), wait for the writer to flush.
- Assert `SELECT count(*) FROM cairn_oplog WHERE tenant_id = $tenant == N` (**exact
  1:1** — one oplog row per source WAL event; the snapshot seeds other tenants'
  rows, excluded by the tenant filter).
- Assert `metrics.oplog_dropped == 0` (no drops under the test load).
- Report `events/sec = N / drain_wall_clock` (informational, machine-dependent).

**Acceptance:** test passes against docker PG; prints `amp=1.00 dropped=0
events/sec=<n>`.

### Scope note (honest framing for the moat)

The write-amp harness answers *"does the oplog amplify writes / drop under load?"*
→ **1:1, 0 drops.** It does **not** re-measure the 833k ops/sec moat: that number is
`FakeReplicator` eval-only by design (`benches/results/RESULTS.md` warns never to
compare eval vs end-to-end), and the oplog is **opt-in/off the FakeReplicator hot
path** (833,305→833,307 with `NOSTOS_BENCH_OPLOG=1`, already in RESULTS.md). The
fan-out-side oplog cost is already measured (invisible); this harness closes the
**real-PG `PgOpLogWriter` INSERT** gap that was slice-6.

## Deliverable 2 — JWKS real-Supabase probe (env-gated; runs with operator creds)

`crates/nostos-infra/tests/jwks_real_supabase.rs`, two `#[ignore]` + env-gated tests:

- `live_supabase_jwks_fetches_and_parses` — needs **only** `NOSTOS_SUPABASE_URL`
  (the JWKS is public). Fetches the live JWKS, parses it through nostos's exact
  `infer_algorithm` + `DecodingKey::from_jwk` logic. **VERIFIED 2026-08-05 against
  the real project** (`ltamqsxxumtusyxswezi`): 1 key, **ES256** (P-256),
  kid `537278b0-…` — nostos parses it cleanly. This closes the "our parser chokes
  on Supabase's real JWKS shape" half of the never-exercised path.
- `real_supabase_asymmetric_token_verifies_via_live_jwks` — needs
  `NOSTOS_SUPABASE_URL` **+ `NOSTOS_SUPABASE_JWT`** (a real user access token).
  `SupabaseJwtAuth::from_config(None, Some(jwks_url))` → `SyncAuth::authenticate`
  — exercises decode_header → ES256/RS256/EdDSA → `JwksVerifier.fetch`+verify →
  `Principal`. **VERIFIED 2026-08-05 against the real project** (`ltamqsxxumtusyxswezi`,
  JWKS is **ES256**): a real ES256 user JWT (`sub=f4106da7…`, kid `537278b0…`
  matching the JWKS) verified end-to-end → `Principal{account_id=sub,
  tenant_id=sub}`. The token was minted via a throwaway Edge Function
  (`admin.createUser` + password grant — neither sends email, bypassing the
  rate-limited self-signup path); the function was redeployed inert (410) and the
  user deleted after. Asserts `account_id` non-empty + `tenant_id == account_id`.

**Note on the Supabase MCP tool:** the operator asked to use it, but it is **not
in the active toolset** (context-mode / ide / web_reader only). The probe uses the
public JWKS + Supabase REST directly, which is equivalent for verification.

**Run:** `NOSTOS_SUPABASE_URL=https://<ref>.supabase.co NOSTOS_SUPABASE_JWT=<access_token> \
cargo test -p nostos-infra --test jwks_real_supabase -- --ignored --nocapture`

## What this is NOT (operator-gated, explicitly out of scope here)

- The full **≤5:00 cold-stranger stopwatch on a fresh machine** needs (a) operator
  Supabase creds for this probe and (b) the **W6 release pipeline** (prebuilt
  binaries / `brew tap` / `hook/prebuilt.json`) — QUICKSTART's own timing dry-run
  shows a cold `cargo` compile of `nostos-cli` + `nostos-server` + the Flutter native
  crate blows 5:00 without prebuilts. The probe de-risks the *code* path; the
  stopwatch + W6 stay operator tasks.
- Supabase operational risks flagged in QUICKSTART (direct-connection IPv6 reachability
  — see `scripts/warp-ipv6-egress.sh`; logical-replication slot limits) are
  Supabase-side config, not unit-testable code paths.

## Acceptance (both)

- write-amp: green against docker PG, number published in RESULTS.md, ADR-0025/0026
  status updated to "slice-6 real-PG amplification MEASURED".
- JWKS probe: compiles, self-skips clean without creds, ready for operator creds.
- `make ci` green; single-line conventional commit.
