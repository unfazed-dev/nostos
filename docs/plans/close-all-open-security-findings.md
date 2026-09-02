# Close all open security findings

Scope: every item still open in `docs/plans/v0-2-0-security-audit.md` after the
2026-09-01/02 pass. The user's instruction was "fix all" — issued *after* the
two policy items were surfaced as their call. That is the decision; both are in
scope.

## Ordering — safest first, breaking last

Each batch gets its own commit and its own `make ci`. A batch that fails
verification does not get carried into the next.

### Batch A — additions, no behaviour change for a valid client

| # | Finding | Fix | Default |
|---|---|---|---|
| 9 | Cross-tenant existence oracle in the write-back reject message | Collapse "different tenant" and "absent" into one indistinguishable rejection | **on** — inert otherwise |
| 3 | Unbounded snapshot (`snapshot_source.rs`, `SELECT *` with no `LIMIT`) | Cap that **rejects loudly**, never truncates (see trap below) | **on**, cap above the bench's 40k |
| 2 | No per-principal connection cap (`DeviceCapReached` is global) | Per-principal cap so one tenant cannot lock out the rest | **on**, generous |

### Batch B — auth hardening

| # | Finding | Fix | Default |
|---|---|---|---|
| 4 | `nbf`/`iss` unvalidated on both verifier paths | `validate_nbf=true`; optional `iss` allowlist. `validate_aud` stays false (load-bearing for Supabase) | nbf **on**; `iss` **opt-in** (allowlist unset = unchecked) |
| 5 | HS256 without `exp` is a permanent credential | Require `exp`; env escape hatch; update fixtures | **on** — opt-in would leave the finding open |
| 6 | Rotated keys stay valid through a JWKS outage | Bounded staleness window on the cache | **on**, window generous enough to ride a short outage |

### Batch C — surface hardening

| # | Finding | Fix | Default |
|---|---|---|---|
| 7 | Unauthenticated metadata routes + permissive CORS | Gate `/schema` + `/rules`; tighten CORS | metadata **on**; **`/metrics` stays open** — gating it silently breaks Prometheus scraping |
| 8 | No admin-token rate limiting | Rate-limit the admin path | **on** |
| 10 | No `Origin` check on WS upgrade | Origin allowlist | **opt-in** — unset = no check, or every existing browser client breaks on upgrade |

### Batch D — predicate semantics

| # | Finding | Fix | Default |
|---|---|---|---|
| 11 | `Not` over an absent column over-delivers (3VL gap) | Make the in-memory evaluator three-valued. **Evaluator only** — the SQL path is already correct | **on** |

### Batch E — the last policy default

| # | Finding | Fix | Default |
|---|---|---|---|
| 1 | `NOSTOS_SLOT_MAX_LAG` defaults to `0` (eviction off) | Non-zero default sized so a normal bad connection survives it | **on** |

## Traps this pass must clear

1. **A bare `LIMIT` on the snapshot re-creates Finding 7.** Truncating the first
   sync silently is exactly the bug just fixed: live fan-out stays correct while
   the initial snapshot is quietly short, and the client cannot tell. The cap
   must reject loudly, not truncate. It must also sit above the throughput
   bench's 40,000-row `bench_apply`, or `make ci` and `make bench` break.
2. **Every new knob gets booted, not unit-tested.** `NOSTOS_INSECURE_ANONYMOUS=1`
   passed its unit test and failed to parse in the real binary. This pass adds
   several knobs across the same clap surface — each one is booted with the
   exact value form the docs claim.
3. **Finding 11 has two implementations.** `Not` is compiled to SQL *and*
   evaluated in memory. Changing one splits live fan-out from snapshot — the
   same path-vs-path shape as Finding 7. The target semantics are Postgres's
   own: `NOT (col = 'x')` over a NULL/absent column excludes the row.

## Rules for this pass

- Every fix lands with one runnable check that fails without it.
- No fix weakens an existing control to make a test pass.
- Breaking changes (5, 1) get an escape hatch and a line in the audit doc.
- `make ci` green per batch; full pg e2e before the final report.

## Consults

- 2026-09-02: advisor overloaded twice, then reached on the third try. Its
  four corrections (snapshot truncation, boot-not-unit-test, the two `Not`
  implementations, per-fix default-on/opt-in) are folded in above.

## Designs settled from the code (not from the audit's summary)

Two of the audit's own pointers were wrong; these are read off the source.

- **Finding 3 — the audit's line number is wrong.** `snapshot_source.rs:143`
  is `prepare_columns`, a metadata-only prepared statement that fetches zero
  rows. The unbounded reads are the `client.query` calls in `snapshot` and
  `snapshot_stream`. Fix: `LIMIT cap + 1`, then refuse if the extra row comes
  back. Fetching one past the cap is what makes a breach *detectable* — a bare
  `LIMIT cap` cannot tell a table of exactly `cap` rows from one of a million.
- **Finding 3 has a second half the audit did not mention.** Every snapshot
  failure is a server-side `warn!` and the subscribe proceeds with live fan-out
  only, so a cap breach would have been invisible to the client — the very
  shape the cap is meant to prevent. The cap therefore rejects the *subscribe*
  (table path, after disconnecting the just-connected session) and emits a
  `stream_error` frame (stream path). Other snapshot errors keep the historic
  warn-and-continue; that residual is now recorded as a new finding.
- **Finding 2 — the cap must clear 32.** One subscribe is one session and a
  socket may hold `MAX_TABLES_PER_SOCKET = 32` tables, so a per-account cap
  near the device count would break ordinary clients. Default 512 (~16 fully
  subscribed devices). The reservation reuses the existing `by_account`
  presence index under DashMap's per-key write guard so check-and-increment is
  atomic, and a refusal rolls the global slot back.
- **Finding 6 — the stale-serve is `jwks.rs:189`**, `cache.keys.get(kid)`
  after a failed fetch leaves `cache.keys` intact. Fix is a maximum staleness
  window, not a change to fetch frequency.
- **Finding 7 — `/schema` is a CLIENT endpoint** ("typed schema for client
  auto-schema"), not an operator one. Gating it behind the admin token would
  break every auto-schema client. Gate it behind the *sync* auth instead, and
  only when the deployment has sync auth configured: an anonymous deployment
  has already opted out of tenant separation, and a real client already holds
  a bearer token. `/metrics` stays open — gating it silently breaks Prometheus.
- **Finding 11 — the two paths already disagree.** Read off both, not inferred:
  - SQL, `snapshot_source.rs:523`: `NOT ({inner})` over `"col"::text = $1`.
    Postgres 3VL sends a NULL column to NULL and **excludes** the row.
  - Memory, `predicate.rs:288`: `Self::Not(inner) => !inner.matches_dyn(..)`,
    and an absent leaf is `false`, so `!false` = **includes** the row.

  So the snapshot and live fan-out already deliver different row sets for the
  same subscription. **The fix is evaluator-only — the SQL side is correct and
  must not be touched**; "move both together" would change a correct path and
  manufacture a fresh divergence.

  Converge on SQL's direction: three-valued `Option<bool>` (`None` = unknown),
  Kleene `And`/`Or`, `Not(None) = None`, top level treats `None` as no-match.
  Only outcome that changes is `Not` over an unknown leaf — every other
  combination lands where it does today (verified against the truth table).

  **Path enumeration redone.** A first grep returned "no matches" while
  `snapshot_source.rs:523` plainly matched — a broken search whose null result
  proved nothing. Re-run: 9 `PredicateExpr::Not` sites, exactly one emitting SQL
  (`snapshot_source.rs:523`), the rest parser/param-binding/tests. `nostos-core`
  does **not** evaluate predicates (its hits are the `matches!` macro), so there
  is no third path.

  **ADR-0012 authorizes this; it does not forbid it.** The pinned test cites the
  ADR, but the ADR lists 3VL as *Deferred* (line 97) and rejects it only as
  "speculative until a real schema demonstrates the `Not(Eq{absent})` edge
  bites" (line 221). An audit finding over-delivery plus a snapshot/live split
  is that demonstration. Line 120 names the upgrade path and its one condition:
  "three-valued logic is a documented future refinement — **recorded here, not
  silently changed**." So the change ships with an ADR-0012 addendum, and
  `not_of_missing_eq_returns_true_pinned_edge` is **rewritten** to assert the new
  semantics and name the disagreement it resolves — not deleted.

  `Param` and cross-type mismatch also become `None`: conservative, never
  over-delivers under `Not`, and the existing param tests assert non-delivery at
  top level, which `None` preserves. `matches_filter_ne`'s hand-rolled
  anti-inversion guard is this same bug patched at one leaf — check whether 3VL
  subsumes it.
- **Finding 8 — penalise failures only, and keep it global.** Constant-time
  compare (`subtle::ct_eq`, `admin_auth.rs:61`) and `MIN_ADMIN_TOKEN_LEN=32`
  already exist; the gap is only that guessing is unlimited. `AdminAuth::check`
  (`admin_auth.rs:75`) is the single entry point, so one place to change.

  **The trap: a lockout keyed on failures is a DoS on the operator.** Anyone who
  can reach the route could spam wrong tokens and lock the real operator out of
  their own admin path — trading a low-severity hardening gap for an
  availability bug, the same bad trade Batch B's JWKS staleness fix had to avoid.
  So the penalty attaches to the *failure*, never to the route: a correct token
  is checked first and succeeds immediately even mid-attack.

  Global, not per-IP: `ConnectInfo` is **not** plumbed (`grep` for
  `into_make_service_with_connect_info` in `main.rs` → no hits), so there is no
  peer address to key on without changing how the server is served — and behind
  a proxy every request shares one IP anyway, which makes per-IP keying both
  costlier and less honest. A dependency for one counter fails the ladder.

  Honest severity: brute-forcing a 32-char token is infeasible with or without
  this. The value is defence-in-depth against a *weak* long token and a
  log/alert signal that someone is trying — not a fix for a live break.
- **Finding 1 — `slot_max_lag` is WAL BYTES, not events.** Eviction costs a
  reconnect and resync, not data loss. 1 GiB is generous for a bad connection
  while still bounding the primary's disk.

## Status

Filled in as batches land.

### Batch A — DONE, `make ci` green

`MAKE_CI_EXIT=0`, 78 binaries, **980 passed, 0 failed, 0 ignored** (was 972 —
the 8 new tests). Commits: cross-tenant message, snapshot cap + per-principal
cap, docs.

Boot-verified as required: `NOSTOS_SNAPSHOT_MAX_ROWS=lots` exits 2 with
`invalid digit found in string`; a numeric value boots.

### Batch B — auth hardening, in progress

- **Finding 5 — `exp` now required on HS256.** `verify_supabase_hs256` takes
  `allow_missing_exp`; `NOSTOS_ALLOW_JWT_WITHOUT_EXP` is the opt-in escape
  hatch for a legacy issuer. This converges HS256 with the JWKS path, which
  already required `exp` via `required_spec_claims`.
  - Broke 5 unit tests + 2 integration fixtures, exactly as predicted. Fixed
    at the *helpers* (`valid_hs256_token`, `hs256_token_from_payload`,
    `mint_jwt` ×2) rather than per-test, so each test keeps its own payload
    shape; `hs256_token_from_payload` injects `exp` only when absent.
- **Finding 4 — `nbf` and `iss`.** `validate_nbf = true` (default-on: a token
  minted for a future window was already usable). `iss` is an allowlist that
  defaults to EMPTY = unchecked — a non-empty default would reject every
  existing deployment's tokens on upgrade. `validate_aud` stays false; it is
  load-bearing for Supabase's `aud:"authenticated"`.
  - **Gap found on re-verification:** `10ebc93` only fixed the JWKS path.
    `verify_supabase_hs256` still checked `exp` alone, and
    `SupabaseJwtAuth::with_issuers` forwarded the allowlist only into
    `JwksVerifier` — so `NOSTOS_JWT_ISSUERS` never applied to HS256 tokens and
    a future-`nbf` HS256 token was usable. The JWKS side also shipped with
    zero tests for `nbf`/`with_issuers`. Closed by
    `fix(auth): enforce nbf and issuer allowlist on HS256 path, add tests for
    both verifiers`: `nbf_and_issuer_ok` in `auth.rs` mirrors
    `jsonwebtoken`'s rules (same `JWT_LEEWAY_SECS`, same "absent iss = not
    in list"), `with_issuers` now stores the list for HS256 as well as JWKS
    (one knob, both paths, no `main.rs` change), and both paths gained
    future-nbf / nbf-within-leeway / wrong-iss / no-allowlist tests.
    Writing the JWKS test exposed a second hole: `jsonwebtoken` 10.x's
    `set_issuer` only compares `iss` when present, so with an allowlist set a
    token that simply omitted `iss` still verified. Fixed in the same commit
    by adding `iss` to `required_spec_claims` whenever the allowlist is
    non-empty.
- **Finding 6 — JWKS staleness ceiling.** `DEFAULT_JWKS_MAX_STALE = 30 min`.
  Past it the cache refuses rather than serving, converting an indefinite
  fail-open on revocation into a bounded one. A cache that never fetched
  successfully is refused on the same rule.

All three knobs wired and **boot-verified against the real binary**, which is
the check that matters — `NOSTOS_INSECURE_ANONYMOUS=1` passed its unit test last
session and still failed to parse:

| env | garbage value | documented value |
|---|---|---|
| `NOSTOS_ALLOW_JWT_WITHOUT_EXP` | `maybe` → refused, lists `1/0, true/false, yes/no, on/off` | `=1` boots |
| `NOSTOS_JWKS_MAX_STALE_SECS` | `soon` → `invalid digit found in string` | `=900` boots |
| `NOSTOS_JWT_ISSUERS` | — (free-form list) | issuer URL boots |

Note on finding 4: the `iss` allowlist had to be wired to a knob to count as
fixed at all. Left as a compile-time field it is always empty, which means
`iss` is never checked and the finding is merely *closeable*, not closed.

### Batch E — finding 1, folded into B's gate

`NOSTOS_SLOT_MAX_LAG` now defaults to **1 GiB** instead of `0` (eviction off).

The reasoning that settles the decision the audit deferred: with eviction off,
one client that connects, acks nothing, and stays connected pins the
replication slot and grows WAL without bound — disk exhaustion on the source
primary, requiring no credentials beyond a valid sync session. Eviction costs a
reconnect and a resync, **not data loss**, so the failure mode of the new
default is strictly milder than the failure mode of the old one. 1 GiB is
deliberately generous: a real client on a bad connection has to fall a gigabyte
of WAL behind before it trips. `0` restores the old behaviour.

Clippy pedantic caught `Duration::from_secs(30 * 60)` on the way through and
required `from_mins(30)`.

### Batch D — finding 11, DONE

3VL landed in `predicate.rs` + an ADR-0012 addendum. `cargo test -p
nostos-domain`: **133 passed, 0 failed.** A subagent sweep of every predicate
test in the workspace predicted exactly **one** flip
(`not_of_missing_eq_returns_true_pinned_edge`) and 31 unaffected — which matched
the truth table and matched the run. The pin was rewritten, not deleted, plus a
new `unknown_survives_negation_at_every_depth` covering double negation, the
tenant-ANDed shape that actually ships, and both Kleene short-circuits.

### Batch C — findings 8, 10, 7

- **8 — admin throttle.** Failure-only penalty in `admin_auth.rs`; a correct
  token is compared first and never delayed, so an attacker cannot lock the
  operator out. `failure_delay` is pure and tested at the cap;
  `a_correct_token_is_never_throttled_and_clears_the_counter` pins the property
  that makes it safe. The compiler found a **second call site** (`ingest.rs:162`)
  that a `main.rs`-only grep had missed.
- **10 — WS origin allowlist.** Opt-in `NOSTOS_WS_ORIGINS`, empty = off. A
  subagent confirmed the shape: exactly one browser client
  (`nostos-ffi-wasm/src/transport.rs:592`), while every native client and all
  ~30 integration tests use `tokio-tungstenite`, which sends **no** `Origin`.
  Hence absent-Origin passes — rejecting it would break every native client and
  stop nobody, since only browsers can be forced to send a truthful one. Tests
  cover the suffix/subdomain/scheme/port bypasses.
- **7 — metadata routes, split by consumer.** `/schema` is a CLIENT endpoint →
  gated on **sync auth**; `/rules` is an OPERATOR endpoint (its only production
  reader is the web admin, which already holds the admin token for the PUT on
  the same path) → gated on the **admin token**. `/metrics` stays open; gating
  it silently breaks Prometheus.

  **My own knob was the silent-failure bug this pass exists to kill.** The first
  version read the env var per-request with `.unwrap_or(false)`, so
  `NOSTOS_PROTECT_METADATA=perhaps` booted with protection OFF while the log line
  claimed the variable was "unset". Caught by boot-verification, not by a test.
  Now a clap arg with `parse_env_bool`, so garbage exits 2 like every other knob.

  **Honest limit, logged at boot:** on `NOSTOS_SYNC_AUTH=none` the `/schema` gate
  is theatre — `AllowAnonymous` accepts the empty token. The server warns rather
  than implying protection it does not have. `/rules` is still genuinely gated
  there, because the admin token is unrelated to sync auth.

## Verification actually run (2026-09-02)

| Suite | Command | Result |
|---|---|---|
| Full gate | `make ci` (fmt + clippy `-D warnings` + full suite) | **exit 0, 78 binaries, 990 passed, 0 failed** |
| Real-Postgres e2e | `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg -- --test-threads=1` | **exit 0, 397 passed, 0 failed** |
| Flutter SDK | `flutter analyze` + `flutter test` | analyze clean; **85 passed** |
| Flutter doc signatures | `python3 sdk/nostos_flutter/scripts/check-doc-signatures.py` | OK |
| Web admin | `npm run check` (svelte-check) | **688 files, 0 errors, 0 warnings** |
| Workspace clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |

### Stress / pressure

| Probe | Command | Result |
|---|---|---|
| Fan-out, recorded config | `nostos-bench --clients 1000 --events 100000` | 100,000,000 delivered, **100,000,000 matched, 0.00% drops**, 252,797 ops/sec |
| Fan-out smoke | `nostos-bench --clients 1000 --events 10000` | 10,000,000 delivered, **0.00% drops**, 254,440 ops/sec |
| Reconnect storm | `nostos-reconnect-storm 2000 1000 4000 4000` | **DRAINS CLEANLY** — post-storm drop **0.00%**, reconnect 3,003 ms |

**The reconnect storm is the real test of the per-principal cap.** It performs
1,000 drop+reconnect cycles against a cap of 512, so a counter that failed to
decrement on disconnect would start refusing halfway through. All 1,000
reconnected. The decrement at `store.rs:238` funnels *every* removal path
(transport disconnect and WAL-bloat eviction), uses `saturating_sub` against
underflow, and is gated on `if let Some(stored) = removed` so a double-remove
cannot double-decrement — read first, then confirmed empirically here.

**Superseded 2026-09-02 (fixed fan-out, commit `d3a49f0`).** After the
sequential + Arc-shared fan-out landed, the same 3-pass native recipe measured
**2,682,508 / 2,618,601 / 2,515,049 ops/sec, 0.00% drops, 100M/100M delivered
in 37–40 s** — median 2,618,601, 3.14× the run-3 median below; headline updated
in RESULTS.md / README / CLAUDE.md / STRATEGY.md. The macOS 10k soak was
inconsistent (64.73% drops at load 10.74, then 0.00% / 2.13M ops/sec at load
4.47), so macOS 10k is still not claimed; the Linux-container 10k (50M/50M,
0.00%) is the authoritative <1% result. Raw: `benches/results/raw/2026-09-02-fixed/`.

**Throughput re-measure + 10k soak — MEASURED 2026-09-02 (run 3).** The 252,797
ops/sec in the table above was taken under host contention and is not the
number; two same-day re-measure attempts were also thrown out as contended
(15–21% drops, VS Code `Code Helper` at 93–231% CPU — records in
`benches/results/remeasure-2026-09-02/CONTENDED.md` and `…-run2/CONTENDED.md`).
Run 3, after a reboot and with the orphan `node` processes killed, completed
all three passes at the baseline's exact config: **833,307 / 833,305 / 833,302
ops/sec, drops 0.0028–0.0029%** — median 833,305, spread 5 ops/sec, within
2 ops/sec of the recorded 833,307 / 0.00% baseline. **Baseline confirmed, no
regression.** The `nostos-bench-10k 10000 5000 60` soak ran twice: 395,520
ops/sec @ 52.54% undelivered and 201,248 ops/sec @ 75.85% undelivered — the
known 10k regime (prior probe ~483k @ ~61.4%), so the <1%-drop-at-10k goal
remains NOT met and the 10k figure is still not citable as throughput; the 2×
soak-to-soak gap is unexplained (no cool-down between soaks is the suspect,
unverified). Full section with env, per-pass load and caveats appended to
`benches/results/RESULTS.md`; raw logs in `benches/results/raw/2026-09-02-run3/`.

**Binary names, for the next agent:** the bench binaries are `nostos-bench`,
`nostos-bench-10k`, `nostos-reconnect-storm`, `nostos-bench-pg-ingest` — *not* the
source-file names (`probe_10k`, `reconnect_storm`). An inventory sweep this pass
reported the source names as binary names and `cargo run --bin reconnect_storm`
fails.

**Do NOT read 252,797 ops/sec as a regression against the recorded 833,307.**
Same host (`unfazed-macbook-air.local`, 10 cores), same profile
(`lto=fat, codegen-units=1`) — but the machine was carrying a **load average of
12.55 on 10 cores** during the run: four runaway copies of the context-mode MCP
plugin (`start.mjs` 1.0.18) were each pegged at ~100% CPU, stealing roughly half
the box. The recorded baseline was taken on an idle machine. Comparing the two
would break the same-conditions rule in `docs/BENCHMARK-METHODOLOGY.md` exactly
the way a cross-stage PowerSync comparison would.

What the run *does* establish, and what contention cannot fake: **100,000,000
events delivered, 100,000,000 matched, 0.00% dropped.** The three-valued
predicate change neither dropped nor mis-matched a single event at 100M scale,
and back-pressure held. Throughput on this machine is **unmeasured**, not
regressed — a clean number needs an idle host.

**The pg e2e was checked for the false-positive, not just the exit code.** All 14
`e2e_pg_*` files self-skip silently when `NOSTOS_E2E_PG` is unset and still report
`ok`, so a green exit proves nothing on its own. `grep -c skipping` over the log
returned **0** — the suite genuinely ran. This is the first session in which
Docker was available for it.

### Two self-inflicted failures worth recording

1. **A "killed run" masquerading as a lint failure.** `ws_contract` stalled at
   `_dyld_start` with a **112 KB** resident footprint — it never reached `main`;
   a loaded Rust+tokio binary is tens of MB. That is macOS Gatekeeper/dyld on an
   external volume, not a deadlock in the new per-principal cap, which was the
   obvious suspect. Running the same binary directly afterwards: exit 0. The
   lesson from last session held — `MAKE_CI_EXIT=2` with `binaries=0` and no
   error text means killed, so read the log before "fixing" anything.
2. **Five integration failures caused by my own verification.** The boot-tests
   spawn `./target/debug/nostos-server` while the suite was rebuilding and
   spawning the same binary. Re-run clean, and `admin_auth` 3×3 stable. Do not
   boot-verify and run the gate at the same time.

### Deliberately NOT bundled

- **A token field for `nostos pull`.** `ProjectConfig` has none, and adding one
  means new config surface, env-vs-file precedence, and a new secret on disk.
  That is a feature, not a security fix, and inventing it under "fix all" is the
  scope creep the instructions warn against. Instead `nostos pull` now fails with
  an error naming `NOSTOS_PROTECT_METADATA` and the workaround, rather than a
  bare 401. **Follow-up, done in `9e2313c`:** `nostos pull --token <TOKEN>`
  with env fallback `NOSTOS_TOKEN` sends `Authorization: Bearer <token>` on
  `GET /schema`. The token is a flag/env value only — still no field in
  `.nostos/config.json`, which is committed — and is never printed. Hardening
  on top: `--token` beats `NOSTOS_TOKEN`; the value is a `Token` newtype whose
  `Debug` and `Display` both print `<redacted>`; and a token is only sent over
  `https://` or to loopback (`127.0.0.0/8`, `::1`, `localhost`) — plain
  `http://` to any other host bails unless `--allow-insecure-token` is passed.
  Each rule has a unit test in `crates/nostos-cli/src/commands/pull.rs`.
- **CORS is unchanged.** `build_cors_layer` still returns
  `CorsLayer::permissive()` on empty `NOSTOS_CORS_ORIGINS`. That is the
  documented local-dev default and `NOSTOS_CORS_ORIGINS` is the existing
  production knob. With `/schema` and `/rules` now authenticable, permissive
  CORS no longer exposes metadata to a browser without credentials — CORS is a
  browser mechanism and never constrained a server-side caller anyway. Do not
  describe finding 7 as "CORS tightened"; it is not.
