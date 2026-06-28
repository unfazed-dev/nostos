# ADR-0009: Ack-driven LSN resume and exactly-once delivery

- **Status:** Accepted (shipped — Tier 0 correctness foundation)
- **Date:** 2026-06-27

## Context

Postgres holds WAL until the replication slot's `confirmed_flush_lsn` passes it.
The original `PgReplicator` advanced that LSN on **every** `XLogData` event —
*before any client received the row*. On reconnect, Postgres would replay from a
position past data the client never saw: **silent data loss**. The Phase-1
kill-criterion ("no data loss, no duplication on mid-LSN crash") was therefore
not actually met. The Postgres logical-replication protocol is explicit: the
subscriber advances `confirmed_flush_lsn` only after it has **flushed and
applied** a batch — never preemptively.

## Decision

Advance the replication slot **ack-driven**, from client ACKs:

1. The transport reads `ClientMessage::Ack { lsn }` frames and stamps each
   session sink's `acked_lsn` (monotonic).
2. `SessionStore::min_acked_lsn()` folds to the **minimum** acked LSN across all
   live sessions — the safe-to-flush point.
3. After each fan-out, `FanOutService::run` calls
   `ReplicatorStream::advance_progress(min_acked)`. The `PgReplicator` feeds that
   to `pgwire-replication`'s `update_applied_lsn` (the worker sends the actual
   standby_status_update wire message on its own `status_interval` / keepalive
   schedule — we only write the correct *value*).
4. A reconnecting client sends `resume_lsn` in its `Subscribe`; the transport
   seeds the sink's ack cursor so the slot won't flush past it and rows ≤ it
   aren't re-delivered.

**Defense-in-depth:** each `TokioEventSink` also carries a bounded (256) ring of
recently-delivered LSNs and a `delivered <= acked` range guard, so an
intra-connection double-delivery from any fan-out race is dropped.

## Rationale

- This is exactly the contract the Postgres streaming-replication protocol
  specifies: `confirmed_flush_lsn` reflects what the subscriber has *applied*,
  not what it has *seen*.
- A per-session server-side dedup window does **not** survive reconnect (a fresh
  `SessionId`/sink is minted each connect), so reconnect-duplicates are LSN-resume's
  job — and `RowOp` apply is already idempotent (Insert/Update = upsert by pk,
  Delete is idempotent). A full server-side txn_id dedup table would be YAGNI;
  the ring is the cheap guard the architecture review asked for.
- The minimum (not the maximum, not an average) is the safe point: the slot must
  not advance past the *slowest* live client, or that client loses data on
  reconnect.

## Consequences

**Positive:** silent-data-loss-on-resume is closed; the Phase-1 kill-criterion
is genuinely met; the contract is testable without a real Postgres (the
`advance_progress` port is no-op on the fake, observable on a recording double).

**Negative:** a permanently-silent client (connected but never ACKing) keeps the
slot from advancing → unbounded WAL retention on the source. **Mitigation:** this
is a known, bounded operational risk; WAL-bloat protection
(`max_slot_wal_keep_size`, age-based forced advance, slow-client eviction) is
deferred to ADR-0016 with explicit config knobs. For now the slot retains WAL
*correctly* (no data loss) — the cost is disk, not correctness.

## Alternatives considered

- **Per-event `update_applied_lsn` (the original bug):** rejected — silent data
  loss on reconnect. This ADR exists because of it.
- **Server-side txn_id dedup table:** rejected — doesn't survive reconnect,
  duplicates LSN-resume's job, and `RowOp` apply is already idempotent. YAGNI.
- **Max-only advance (fastest client wins):** rejected — loses the slowest
  client's data on reconnect. The minimum is the only safe point.

## References

- Postgres logical-replication protocol: `standby_status_update` /
  `confirmed_flush_lsn` semantics.
- `pgwire-replication` 0.3.2: `update_applied_lsn` writes a shared progress
  atomic; the worker sends wire feedback on `status_interval` + keepalive.
- Code: `crates/nostos-infra/src/replicator/pg.rs` (`advance_progress`),
  `crates/nostos-infra/src/router.rs` (`TokioEventSink` ack + dedup),
  `crates/nostos-application/src/fanout.rs` (`run` loop).
