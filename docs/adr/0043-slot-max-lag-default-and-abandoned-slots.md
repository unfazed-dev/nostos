# ADR-0043: `NOSTOS_SLOT_MAX_LAG` defaults to 1 GiB; abandoned slots are a separate failure mode

**Status:** Accepted (2026-09-02). Records a decision that shipped in commit
`10ebc93` (v0.2.0 audit, finding 1) without its own ADR, and adds the guard
rails around it. Supersedes the "OFF by default" clause of ADR-0016.

## Context

ADR-0009 made resume correct by advancing the replication slot's
`confirmed_flush_lsn` only as far as the *slowest live client* has acked.
ADR-0016 added the cost-control for that: `EvictionPolicy` disconnects the
slowest session when `head - slowest_acked > max_lag`, and shipped it
**OFF by default** (`NOSTOS_SLOT_MAX_LAG=0`) with "a production deploy MUST set
it" in the docs.

The v0.2.0 security audit (`docs/plans/v0-2-0-security-audit.md`, finding 1 /
"Left deliberately" §2) called this the one genuinely open disk-exhaustion
exposure: any holder of a valid sync session can connect, ack nothing, and pin
WAL on the operator's primary until the disk fills. No extra credentials
needed. The audit declined to pick a default on its own because a non-zero
value "disconnects real users". This ADR makes that call.

Two things had to be true before a non-zero default was safe:

1. **Enforcement must never drop the slot.** Read
   `crates/nostos-application/src/fanout.rs` (`should_evict` → 
   `store.remove(id)`): eviction removes the slowest *session* from the
   `SessionStore`. The client reconnects and resumes (op-log replay inside the
   ADR-0025 window, snapshot-reconcile outside it). The Postgres slot is never
   touched; there is no slot-drop path in the fan-out at all. The worst case
   is one reconnect + resync for the one client that fell a gigabyte behind —
   never data loss, never a dropped slot under a legitimate backlog.
2. **The real-PG e2e must not change.** The e2e clients ack promptly and
   move kilobytes, not gigabytes; the suite ran green on `10ebc93`.

Both hold, so **Option 1** (non-zero default) is the safer of the two the
audit offered — Option 2 (keep `0`, warn) leaves the exposure in place for
every operator who doesn't read the log.

## Decision

1. **`NOSTOS_SLOT_MAX_LAG` defaults to `1073741824` (1 GiB).** A live client
   more than 1 GiB of WAL behind the head is evicted (disconnected; it
   reconnects). 1 GiB is deliberately generous: a phone on a bad train
   connection has to miss a gigabyte of *its subscribed* changes before it
   trips, and the price when it does is one resync. WAL retention past that
   on a sync backend is a bug or an attack, not a workload.
2. **`0` still means unbounded**, but nostos-server now logs a startup `warn!`
   naming the knob, the risk, and the companion Postgres setting. Opting out
   is allowed; opting out silently is not.
3. **Eviction and slot-WAL bounding are two different failure modes and the
   docs say so.** Eviction protects the primary *while nostos-server is
   running*. A slot left behind by a server that is gone (crash, decommission,
   renamed `NOSTOS_PG_SLOT`) has no sessions to evict and pins WAL forever —
   the four abandoned slots the audit found on the dev database
   (`cairn_slot`, `cairn_slot_arxa_kit`, `atlet_rt_sim_slot`,
   `atlet_demo_slot`, ~120 MB each) are exactly this case, and no
   `NOSTOS_SLOT_MAX_LAG` value would have helped. The only bound for that is
   Postgres `max_slot_wal_keep_size` (`NOSTOS_PG_SLOT_WAL_KEEP_SIZE` sets it
   on the slot; `ALTER SYSTEM` sets it server-wide).
4. **`nostos doctor` reads `max_slot_wal_keep_size`** and prints an advisory
   (⚠, does not fail the run) with the exact `ALTER SYSTEM` line when it is
   `-1`. Advisory rather than blocking because the dev compose Postgres ships
   unbounded and that is correct on a laptop; a blocking check would train
   people to ignore doctor.
5. **The default is pinned by unit tests** (`slot_max_lag_tests` in
   `crates/nostos-server/src/main.rs`): default parses to 1 GiB, `0` maps to
   `EvictionPolicy::disabled()`, an explicit value is honoured. The library
   default (`EvictionPolicy::default()`) stays disabled so a bare
   `FanOutService` in benchmarks and unit tests never evicts.
6. **Threshold is chosen, not measured.** 1 GiB is a conservative guess; no
   lag data from a real tenant exists yet. Revisit trigger: p99 session lag
   from `cairn_slot_lag_bytes` over ≥1 week of a real tenant (that metric is
   not emitted yet — adding it is the prerequisite for the revisit).

## Consequences

**Positive:** an unconfigured deploy is no longer a credential-free
disk-exhaustion target. The v0.2.0 audit's last "genuinely open" item closes.
Operators who read nothing get a safe default; operators who set `0` get told
what they gave up.

**Negative:** a client that legitimately accumulates >1 GiB of lag (very
high-write table, very long offline window, resumed on a slow link) gets one
forced reconnect. If that turns into a reconnect storm for a real workload,
the fix is a higher explicit value or back-off in the eviction loop — not a
return to unbounded.

**Not covered here:** abandoned-slot cleanup (dropping slots nobody owns) is
still an operator action. `nostos doctor` names the setting that bounds their
cost; it does not drop them.

## References

- ADR-0009 (ack-driven slot advance), ADR-0016 (eviction policy + 
  `max_slot_wal_keep_size` knob), ADR-0025 (op-log replay window).
- `docs/plans/v0-2-0-security-audit.md` finding 1, "Left deliberately" §2.
- `docs/OPERATING.md` §1 (config table), §3.4 (slot invalidation).
- Code: `crates/nostos-server/src/main.rs` (`slot_max_lag`, `eviction_policy`,
  startup warn), `crates/nostos-application/src/eviction.rs`,
  `crates/nostos-cli/src/commands/doctor.rs`.
