# ADR-0042: Mirror ingest for the desktop-sidecar topology

- **Status:** Accepted (2026-08-30)
- **Context:** arxa's B2 decision (arxa `docs/plans/doorbell-decision-2026-08-29.md`
  §4) makes the studio engine the single WRITER of a mirrored read-model
  (approvals table first) replicated by a nostos-server running as a
  Tauri-supervised sidecar on the desktop. Nostos's only data sources were the
  Postgres logical-replication stream (`PgReplicator`) and the synthetic
  `FakeReplicator`; client write-back (ADR-0013) requires the pg replicator
  and a real Postgres. The sidecar must run with NO database.

## Decision

1. **Channel-fed replicator.** `MirrorReplicator` (nostos-infra
   `replicator/mirror.rs`) implements `ReplicatorStream` over a tokio mpsc
   channel — the adapter-swap seam main.rs:8-11 advertises. Selected with
   `NOSTOS_REPLICATOR=mirror`.
2. **One write door: `POST /ingest`.** Admin-token gated exactly like
   `PUT /rules` (fail-closed 404 when `NOSTOS_ADMIN_TOKEN` is unset; 401
   before any body parse; Content-Type discipline). The batch is validated
   all-or-nothing, then applied; the response echoes the server-stamped LSNs.
   Strict shapes: `upsert` carries the full row image (its `"id"` column is
   the pk — the v1 convention, so pk/payload cannot diverge); `delete`
   carries only the pk; unknown ops are rejected (the writer is ours).
3. **In-memory snapshot buffer with a shared LSN allocator.** The
   `MirrorHandle` records every ingested row (upsert/delete) into a
   per-table BTreeMap and implements `SnapshotSource` from it, so a
   freshly-subscribing client sees pre-ingest rows. Live events and
   snapshot bands draw from ONE monotonic allocator, and a snapshot
   reserves its band atomically — snapshot LSNs can therefore never
   collide with (and cause the per-session dedup/gate to drop) a live
   event, closing the corner `snapshot_source.rs::rows_to_events` carries
   as a ponytail for the pg path. Tenant scoping (ADR-0011/0018 parity) is
   fail-closed: a row without the tenant column matching the principal's
   scope value is not visible in snapshots.
4. **No write-back.** Under mirror the engine is the only writer; clients
   keep `NoWriteBack` (same as fake mode).

## Alternatives rejected

- **WS write-back via a client connection (ADR-0013 frames):** requires
  `NOSTOS_REPLICATOR=pg` + a real Postgres (`NoWriteBack` otherwise,
  main.rs), and round-trips every mirrored row through PG before mobile
  sees it. The whole point of the sidecar is no database on the desktop.
- **Local Postgres on the desktop + stock pg path:** maximum server-side
  reuse but bundles and supervises a database per desktop install — an
  ops cost B2 explicitly avoids (pushd precedent: one static binary,
  SQLite or nothing).
- **Mirror-out through a shared file / SQLite WAL tailing:** a bespoke
  replication protocol with none of nostos's ordering or snapshot
  machinery; strictly more code than the channel adapter for less
  correctness.

## Consequences

- The snapshot buffer is in-memory only: a sidecar restart resets rows and
  LSNs; clients re-subscribe via the wire's epoch semantics and re-snapshot.
  Correct for phase-1 approvals (ephemeral, few); revisit with the
  full-mirror phases if a durable read-model is ever required.
- LSNs are dense per boot and reset to 1 — a resumed client whose
  `resume_lsn` predates the restart is handled by the epoch mismatch
  (full re-subscribe), never by stale-row silently winning.
- The engine-side mirror-out writer (arxa-studio) POSTs to this route over
  localhost; until it lands, the route is exercised by the rig directly.
- The route exists only when the mirror replicator is selected (the
  composition root constructs the handle there); in every other mode the
  path is a genuine 404.
- **Verified end-to-end (2026-08-30, real binary, rig-only WS harness
  `/tmp/arxa-d68/task6_ws.sh`):** ingest 200s carry server-stamped LSNs;
  a subscriber connecting AFTER two pre-ingests receives both inside
  `snapshot_begin`/`snapshot_end`; a third post-subscribe ingest arrives
  live with its ingest LSN. Three wire facts phase-1b client work must
  know: (1) snapshot rows arrive as ONE JSON-array frame between the
  boundary control frames — the live path stays single-frame; (2)
  snapshot rows are LSN RE-STAMPED at materialization (ingest acked
  `[1,2]`, the wire carried `[3,4]` for the same rows, while the
  post-subscribe live event kept its acked LSN) — ingest-ack LSNs are
  NOT stable row identifiers; resume/dedup must key off wire LSNs plus
  the epoch, never ingest acks; (3) the ruleset gates sync tables — a
  subscribe for a table outside `nostos_rules.toml` is a silent 1008
  close, so the sidecar must ship a rules file containing `approvals`
  (or run rules-less, which is all-mode).
