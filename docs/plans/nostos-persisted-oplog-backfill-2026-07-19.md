# Persisted Op-Log Backfill — Design + Slices

**Date:** 2026-07-19 · **Status:** plan, awaiting operator sign-off
· **ADR:** 0025 (to be written as slice 0)
· **Preceded by:** `docs/plans/reconnect-glitch-fix-2026-07-19.md` (Phase 1 replay-on-top shipped; Phase 2 Piece A premise falsified — see below)

## Context + decision

The ratified Phase 2 (epoch gate → "resume without snapshot") was **falsified** 2026-07-19: nostos has no client WAL backfill (per-session sink doesn't survive reconnect; the unconditional current-time snapshot is the only catch-up), so skipping the snapshot on epoch-match would silently drop offline-gap changes in multi-user. In the same investigation a **second P0** surfaced: the snapshot is present-rows-only upserts with no reconcile, so a row hard-deleted server-side while the client is offline persists client-side as a stale orphan (hard-delete schema; untested; invisible in single-user).

Five web-research agents (2026-07-19) converge: for Postgres-logical-replication engines, **backfill the op-stream is the industry-correct mechanism** (ElectricSQL ships this way, among others); **filtered-snapshot `WHERE updated_at > X` is the textbook anti-pattern** (drops deletes); **hard-delete is the correct convention** (the WAL already carries DELETEs via pgoutput + replica identity — tombstones are redundant server-authoritative); and **every production engine persists its op-log to disk/DB** (comparable engines' bucket op-log tables in Postgres, CouchDB by-seq, ElectricSQL shape logs). The consultant's "filtered-snapshot" pivot is rejected (it lacked the delete-gap context).

**Operator decision (2026-07-19): build the persisted op-log (production-grade).** This is a subset of the bucket-based op-log design used by comparable sync engines — a simpler single-stream variant, architecturally aligned, diverging only in the deferred scalability machinery (per-bucket partitioning + checksums = the 142k ops/sec moat, cited explicitly, not conflated with the resume primitive).

## Architecture (four mechanisms)

1. **Persisted op-log** (`cairn_oplog` Postgres table) — written at the `FanOutService::run`-loop chokepoint (`crates/nostos-application/src/fanout.rs`, the `self.fan_out(event)` call). Columns: `op_id BIGSERIAL`, `lsn BIGINT`, `table_name TEXT`, `pk TEXT`, `op TEXT` (upsert/delete), `payload JSONB`, `tenant_id TEXT`, `created_at TIMESTAMPTZ DEFAULT now()`. Indexed `(tenant_id, lsn)` for replay + `(table_name, pk)` for compaction. Batched writes (one tx per commit boundary) to bound write-amplification.
2. **Replay-on-reconnect** (`register_subscribe`, `crates/nostos-infra/src/transport.rs:546`) — on subscribe, if `client_epoch == server_epoch` AND `client_checkpoint ≥ oplog_window_tail`: replay `SELECT ... FROM cairn_oplog WHERE tenant_id = ? AND lsn > ? ORDER BY lsn` to the fresh per-session sink, then **atomically hand off** to live fan-out at the replay-end LSN. Else → snapshot path. Delivers missed INSERTs/UPDATEs/**DELETEs** — fixing the offline-gap + offline-delete cases in-window.
3. **Snapshot-reconcile fallback** (nostos-domain `WireFrame` boundary marker + nostos-client `apply`) — a begin/end marker on snapshot delivery; the client removes local rows absent from a complete snapshot. Fixes the offline-delete orphan P0 for the snapshot path (long gap / first-connect / epoch-mismatch). This is the same semantics as Replicache's Reset / comparable engines' checkpoint-reconcile pattern.
4. **Epoch gate** (nostos-infra `PgReplicator` + `SyncRouterState`) — `slot_epoch: Arc<AtomicU64>` bumped on slot (re)creation (the P0-1 `Lost` branch, `pg.rs:569-604` + fresh-create), shared via a new `with_epoch` builder (`SyncRouterState`, `transport.rs:68-100`). The replicator is currently `mem::forget`-detached (`main.rs:319-335`); thread the counter without disturbing the driver. `client_epoch != server_epoch → SNAPSHOT` (can't backfill from a recreated slot's dead lineage).

## Slices (each `make ci`-gated; committed independently)

- **Slice 0 — ADR-0025 + this plan.** Records the decision + research backing.
- **Slice 1 — Snapshot-reconcile (the P0 fix, independent, ships first).** `WireFrame` boundary marker (backward-compatible `#[serde(default)]`); nostos-client reconcile-on-complete-snapshot (remove orphans). **Test:** server hard-deletes while client offline → reconnect → client's local row removed. Fixes the offline-delete P0 even if backfill is deferred.
- **Slice 2 — Op-log schema + writer.** `cairn_oplog` migration (idempotent `CREATE TABLE IF NOT EXISTS`); `OpLogWriter` at the fan-out chokepoint, batched per commit boundary. No behavior change (writes alongside existing fan-out). **Test:** WAL events land in `cairn_oplog` with correct `(lsn, table, pk, op, payload, tenant)`.
- **Slice 3 — Epoch gate.** `slot_epoch` counter + `with_epoch` builder; predicate in `register_subscribe`. **Test:** slot dropped+recreated → client re-snapshots (extends `e2e_pg_slot_invalidation`).
- **Slice 4 — Replay-on-reconnect.** The backfill: replay `cairn_oplog` from checkpoint to the fresh sink, atomic handoff to live. **Test:** client offline, server changes (insert/update/**delete**), reconnect → client receives missed ops via replay, no re-snapshot; long gap (> window) → snapshot-reconcile.
- **Slice 5 — Compaction.** Background collapse of multiple ops on `(table, pk)` → current value; retention window. **Test:** compacted op-log; old checkpoint → snapshot, recent → replay.
- **Slice 6 — Restart-resilience e2e (real-PG).** Server restart mid-offline-client → op-log persists → client resumes from checkpoint without full re-snapshot. The whole point of persisted-vs-in-memory.

## Test plan (gates)

- **P0 offline-delete** (slice 1): the orphan bug fixed via reconcile.
- **Backfill incl. deletes** (slice 4): the resume correctness (in-window).
- **Restart-resilience** (slice 6): persisted op-log survives restart (the production-grade property chosen over the in-memory ring).
- **Epoch/recreate** (slice 3): slot-loss → snapshot (P0-1 preserved).
- **Compaction** (slice 5): bounded op-log; correct replay after compaction.
- Re-benchmark (`make bench`): the op-log write per event adds write-amplification — must re-measure the 142k ops/sec claim; ship before/after or revert (Tier-5 precedent).

## Risks + tradeoffs

- **Write-amplification** (HIGH): every WAL event → PG WAL + `cairn_oplog` row. Batched per-commit mitigates; re-bench is mandatory (the moat number is load-bearing for positioning).
- **Replay-to-live handoff race** (MEDIUM): the atomic boundary between op-log replay and live fan-out — careful design + a chaos test.
- **Tenant isolation** (MEDIUM): `tenant_id` indexing + enforcement; no cross-tenant op leakage (mirrors ADR-0018 write-path tenant enforcement).
- **Wire-format change** (LOW): boundary marker via `#[serde(default)]` keeps old clients working.
- **Compaction correctness** (MEDIUM): collapsing ops must preserve net effect; tombstone-aware.
- **Scope** (accepted): this is nostos's core sync data plane — weeks, not days. Slice 1 ships the P0 fix independently so value lands before the full backfill.

## Divergence from bucket-based sync engines (deferred moat machinery — cited, not built here)

- **No per-bucket partitioning** — single-stream replay (O(window)), not O(changed-buckets). Buckets = the multi-tenant fan-out efficiency that powers the 142k ops/sec @ 1k clients claim.
- **No per-bucket checksums** — full op replay, no `checkpoint_diff` checksum-diffing.
- These are the scalability/moat layer; deferred to a later phase and tracked as the explicit divergence. The resume *primitive* (this plan) is correctness-bearing and aligned; the bucket machinery is performance-bearing and the moat.

## Research sources (🔥 primary)

ElectricSQL Shapes (streamed DELETEs, persisted shape log, resume-any-time); Replicache Reset/Global-Version (full-replace fallback + `del` op); CouchDB/Cloudant (tombstones, peer-to-peer only); Postgres logical-replication + `pg_replication_slots` + `REPLICA IDENTITY`; Fivetran/Nango (timestamp cursors miss deletes — the anti-pattern). Full agent transcripts: `aa4e395d2cb70fda1` (reconcile), `a371925a0faf305b7` (filtered-snapshot), `a7c4c6233d3db9e32` (backfill retention), `aad0941b92ed8d6e7` (delete convention), `ab2bbe6eee3626413` (bucket-log design deep-dive).
