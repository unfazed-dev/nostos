# ADR-0025: Persisted operation-log backfill for reconnect resume

**Status:** Accepted · **Date:** 2026-07-19

## Context

nostos's reconnect catch-up is an unconditional current-time `SELECT` snapshot on every subscribe (`PgSnapshotter`, `crates/nostos-infra/src/snapshot_source.rs`). There is no client WAL backfill: the server never inspects `resume_lsn` for resume (`register_subscribe`, `transport.rs:546` is unconditional), the replicator is shared (one PG slot → fan-out, cannot seek per-client), and the per-session sink's dedup ring "does not survive reconnect" (`router.rs:41`).

Two gaps follow:

1. **The ratified Phase 2 Piece A (epoch gate → "resume without snapshot") is unsafe** — skipping the snapshot on epoch-match would silently drop every server-side change made while the client was offline. Invisible in the single-writer demo; data-loss in multi-user (the launch target).
2. **Offline deletes orphan client-side (P0)** — the snapshot is present-rows-only upserts with no reconcile; a row hard-deleted server-side while the client is offline persists client-side as a stale orphan. The demo uses hard deletes; no test covers it. **[RESOLVED 2026-08-04]** — slice-1's snapshot-reconcile closes this on the *client apply path*: the server brackets each snapshot with `snapshot_begin`/`snapshot_end` control frames (`transport.rs:717,726`), the client pump intercepts them before row-decode exempting pending outbox PKs (`client.rs:1018`), and the apply engine reaps local PKs absent from the snapshot via `delete_pks` (`apply.rs:272-298`, `sqlite.rs:766`). Verified by `snapshot_reconcile_removes_orphans_absent_from_snapshot` (`apply.rs:597`), green in the 468-test workspace run.

Five web-research agents (2026-07-19) converge on the industry convention for Postgres-logical-replication engines: **backfill the op-stream** is the correct mechanism (PowerSync, ElectricSQL ship this way); **filtered-snapshot `WHERE updated_at > X`** is the textbook anti-pattern (drops deletes); **hard-delete is correct** (pgoutput already carries DELETEs via replica identity — tombstones are redundant server-authoritative); and **every production engine persists its op-log to disk/DB** (PowerSync bucket table in Postgres, CouchDB by-seq, ElectricSQL shape logs). Bounded in-memory retention is correct but restart-fragile (deploy → thundering herd of re-snapshots).

## Decision

Build the **persisted operation-log backfill** — a single-stream subset of PowerSync's bucket model. Four mechanisms:

1. **Persisted op-log** (`cairn_oplog` Postgres table) written at the `FanOutService::run`-loop chokepoint, batched per commit boundary; indexed `(tenant_id, lsn)` for replay + `(table_name, pk)` for compaction.
2. **Replay-on-reconnect** — on subscribe, if `client_epoch == server_epoch` and the client checkpoint is within the op-log window, replay `cairn_oplog` from the checkpoint to the fresh sink then atomically hand off to live fan-out. Delivers missed INSERTs/UPDATEs/DELETEs.
3. **Snapshot-reconcile fallback** — a begin/end boundary marker on snapshot delivery; the client removes local rows absent from a complete snapshot. Fixes the offline-delete P0 for the snapshot path (long gap / first-connect / epoch-mismatch).
4. **Epoch gate** — `slot_epoch: Arc<AtomicU64>` bumped on slot (re)creation, shared between `PgReplicator` and `SyncRouterState`; `client_epoch != server_epoch → SNAPSHOT` (cannot backfill from a recreated slot's dead lineage).

Implementation: `docs/plans/nostos-persisted-oplog-backfill-2026-07-19.md` (7 ci-gated slices; slice 1 = snapshot-reconcile ships the P0 fix independently first).

## Consequences

- **Positive:** production-grade reconnect resume; offline-delete correctness; restart-resilience (the property that distinguishes persisted from in-memory); hard-delete stays the convention (no `deleted_at` columns required).
- **Negative — write-amplification:** every WAL event now also writes a `cairn_oplog` row. `make bench` must re-measure the 142k ops/sec moat claim; ship before/after or revert (Tier-5 precedent, `docs/ROADMAP.md`). Batched per-commit writes mitigate.
- **Risk:** replay-to-live handoff race (atomic boundary); tenant isolation in the op-log (mirrors ADR-0018); compaction correctness (collapse preserves net effect).
- **Divergence from PowerSync (deferred, cited):** no per-bucket partitioning + no per-bucket checksums. Those are the multi-tenant fan-out efficiency that powers the 142k ops/sec @ 1k clients moat — the scalability/moat machinery, tracked separately, not conflated with the resume primitive ratified here.

## References

- Plan: `docs/plans/nostos-persisted-oplog-backfill-2026-07-19.md`
- Research: agent transcripts `aa4e395d2cb70fda1`, `a371925a0faf305b7`, `a7c4c6233d3db9e32`, `aad0941b92ed8d6e7`, `ab2bbe6eee3626413` (2026-07-19)
- Prior: ADR-0009 (ack-driven LSN resume), ADR-0013 (write-back), ADR-0014 (tiered conflict resolution), ADR-0016 (client + WAL bloat)
- Supersedes the "resume-without-snapshot" framing of `docs/plans/reconnect-glitch-fix-2026-07-19.md` Piece A (premise falsified 2026-07-19)
