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
| 11 | `Not` over an absent column over-delivers (3VL gap) | Absent column under `Not` must not match — **both** the SQL compiler and the in-memory evaluator, moved together | **on** |

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
- **Finding 11 — the two paths already disagree.** Postgres evaluates
  `NOT (col = 'x')` over NULL to NULL and excludes the row; the in-memory
  `matches_dyn` is two-valued and returns `true`. Converging them means making
  the evaluator three-valued (`Option<bool>`, `None` = unknown, top level
  treats unknown as no-match), which is what SQL already does — not inventing
  a third semantics.
- **Finding 1 — `slot_max_lag` is WAL BYTES, not events.** Eviction costs a
  reconnect and resync, not data loss. 1 GiB is generous for a bad connection
  while still bounding the primary's disk.

## Status

Filled in as batches land.

- Batch A implemented; `make ci` running.
