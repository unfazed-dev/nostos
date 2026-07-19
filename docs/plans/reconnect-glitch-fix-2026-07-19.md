# Reconnect UI Glitch — Fix Design

**Date:** 2026-07-19 · **Status:** Phase 1 implementing; Phase 2 tracked
· **Method:** code root-cause + 2 web-research agents + architecture consultant (conf HIGH)

## The bug (operator-reported)
On reconnect (offline → online), the list UI **briefly flashes the server's
data** (ms) before settling back to the local state. Locally-deleted rows
reappear; locally-modified rows revert to the old value; locally-**added** rows
persist. Glitch window = outbox-flush round-trip.

## Root cause (conclusive from code)
1. **Server delivers a fresh full-table snapshot on EVERY subscribe** — including
   reconnects with a non-zero `resume_lsn` (`crates/nostos-infra/src/transport.rs:551`
   `register_subscribe`; snapshot not gated on fresh-vs-resuming). Violates
   ADR-0009 (resuming clients should get incremental WAL fan-out, not a snapshot).
2. **Snapshot lands before `flush_outbox`** (`crates/nostos-client/src/client.rs:565-610`:
   subscribe sent, server snapshot delivered into sink, THEN outbox drains).
3. **`apply_batch` is per-row UPSERT/DELETE** (`sqlite.rs:373-414`), not
   full-table-replace — so added rows survive, but deleted/modified rows get
   clobbered by the server's (stale, edit-less) image.
4. **No pending-write protection** — `apply_batch` unconditionally overwrites;
   nothing shields rows with un-acked outbox writes.

## Research synthesis (industry best practice, July 2026)

### The unanimous pattern — keep the client cursor OPAQUE
Every mature sync engine (Replicache cookie, PowerSync operation_id, CouchDB
sequence) keeps the **client cursor in a separate space from the DB's WAL LSN,
opaque to the client; the server is the sole arbiter of resumability.** nostos's
trap — letting a synthetic LSN participate in `resume_lsn < slot.restart_lsn` —
"is not documented as such precisely because the standard pattern makes it
impossible by construction." 🔥

### The flash-fix pattern — write-checkpoint barrier (PowerSync)
*"While mutations are present in the upload queue, the client does not advance
to a new checkpoint… the client never has to resolve conflicts locally."* 🔥🔥
Gate applying incoming server data on the outbox being reconciled. Variants:
- **Write-checkpoint barrier** (PowerSync): buffer incoming until outbox drains + acks.
- **Replay-on-top** (Replicache): apply server data, replay pending mutations on top, reveal. Simpler — no buffering.

### Snapshot-vs-resume — resume is the norm, not the exception
Mature engines **resume from a cursor**; full re-snapshot is reserved for
first-sync / schema-change / **slot-or-checkpoint invalidation**. 🔥 The
canonical Postgres resumability predicate: `wal_status IN ('reserved',
'extended','unreserved') AND invalidation_reason IS NULL AND
confirmed_flush_lsn >= restart_lsn`. `lost` / `wal_removed` → must
drop+recreate+re-snapshot.

## The correct fix = two independent pieces

### Piece A — epoch-based snapshot-vs-resume gate (server) [Phase 2, tracked]
Make the client cursor a **`(slot_epoch, synthetic_lsn)` tuple**, where
`slot_epoch` is a server-issued generation counter that bumps whenever the slot
is (re)created. Server predicate on subscribe:
```
cursor.slot_epoch != current_epoch    → SNAPSHOT   # slot recreated (P0-1) ✓
wal_status == 'lost'                  → SNAPSHOT
invalidation_reason == 'wal_removed'  → SNAPSHOT
else                                  → RESUME      # no snapshot
```
**One mechanism solves two problems:** the epoch check is the primary gate (no
numeric LSN comparison → sidesteps the synthetic-vs-real-WAL space), AND a
recreated slot's new epoch forces the snapshot (P0-1 preserved). *Cannot be
shortcut* — without the epoch, a numeric comparison crosses LSN spaces.

Scope: multi-crate (PgReplicator/PgSnapshotter track epoch; transport carries
it; client cursor stores it; `register_subscribe` predicate). Needs a real-PG
slot-loss integration test. The bandwidth + ADR-0009 fix.

### Piece B — client write-checkpoint barrier [Phase 1, now]
**Replay-on-top variant** (chosen for Phase 1 — simpler, no buffering, zero
P0-1 risk): after each `apply_batch(...)` in the client receive loop,
re-apply the pending outbox writes locally **before the broadcast tick** so the
optimistic state is always on top when the watch emits. Eliminates the flash.

Scope: client-side only (`crates/nostos-client`). Additive — cannot break resume
or slot-loss (worst case a no-op re-apply). Matches PowerSync's template per the
research. Demo-ready.

## Phasing decision (operator-confirmed 2026-07-19)
- **Phase 1 = Piece B now** (client replay-on-top barrier): kills the reported
  flash, zero server change, zero P0-1 risk. Demo-ready.
- **Phase 2 = Piece A** (epoch-based snapshot gate): tracked as the pre-launch
  bandwidth + ADR-0009 + synthetic-LSN-correctness fix. Task #15.

This is **two correct pieces sequenced by urgency**, not a band-aid-then-fix:
both are named standard patterns in the research. Piece B fully resolves the
reported symptom; Piece A closes the separate bandwidth/ADR-0009 gap.

## Sources (🔥 primary / 🌡️ secondary)
PostgreSQL `pg_replication_slots` + logical-decoding docs; Morling on
confirmed_flush_lsn vs restart_lsn; PowerSync Protocol + Consistency (write
checkpoints) + Service Architecture; Replicache How-It-Works (cookie,
lastMutationID, pending discard) + Global/Row-Version strategies; CouchDB
Replication Protocol §2.4.2.3.3 (common-ancestry → full replication); Weidner
"Server Reconciliation" (2024); Kleppmann/Ink&Switch local-first; Fivetran/Estuary/PeerDB
on slot-invalidation → resync. Full lists in research agent transcripts
(`a2815ceda45d34cc3` PG-resume, `ab0295c46586dd47c` protection-patterns).

## Consultant
Architecture-domain consult (GLM-5.2, conf HIGH) recommended D' (Piece A);
this design refines it into the two-piece form after the research revealed the
synthetic-LSN correctness constraint + that the flash fix (Piece B) is
independent and standard.
