# ADR-0026: Channel-authority + producer-before-consumer shutdown for op-log durability

**Status:** Accepted · **Date:** 2026-07-20

## Context

ADR-0025 slice-6 (commit `e395dea`, `feat(sync): graceful oplog writer shutdown drains in-flight batch`) added `PgOpLogWriter::shutdown()` so a SIGTERM drains the in-flight batch instead of dropping the last `≤ BATCH_MAX` entries mid-INSERT. A regression test written against that drain then **confirmed a residual P1**: during the shutdown final-flush, a late `try_send` from the still-running `PgReplicator` returned `Ok(())` (the channel accepted the entry) → the DELETE was buffered in the mpsc receiver queue → the flush loop had already drained once and was mid-final-INSERT → the buffered entry was lost on task exit. On a matching-epoch reconnect (ADR-0025 F2 `resume_info{epoch}` → `Storage::save_epoch`), the server **skips the snapshot**, so slice-1 reconcile (`nostos-core/src/apply.rs:597`, snapshot-only) never runs → the client never sees the DELETE → **ghost row**. The pre-fix probe lived at `oplog.rs:985` as `drain_boundary_late_append_during_final_flush_is_lost` and demonstrated the race via `late_result.is_ok()`.

This is the **same silent-data-loss category as the original slot-invalidation P0** (`nostos-soundness-audit-2026-07-19`, issue #1): the system reports success (`try_send` returned `Ok(())`, the writer task "shut down cleanly") while a DELETE disappears. Invisible in the single-writer demo; data-loss in multi-user (the launch target). The slot P0 dropped deletes by failing to invalidate a stale slot lineage; this race drops deletes by accepting them into a channel whose consumer is already draining. Either way the matching-epoch replay path is the amplifier — F2's snapshot-skip turns a missed DELETE into a permanent ghost row.

## Decision

Ship **both** fixes, defense-in-depth. They guard different callers and different failure shapes; either alone leaves a residual vector.

### Fix A — producer-before-consumer shutdown ordering (`crates/nostos-server/src/main.rs`)

Retain the pg replicator's `JoinHandle` (was `std::mem::forget` detached) and `.abort()` it **between** axum's graceful drain and `OpLogWriter::shutdown()`. The detached replicator is the producer; the op-log drain is the consumer. Stopping the producer first closes the window in which a late replication event can race into the final flush. The fake branch stays detached (`mem::forget`) — it's the bench/demo path, no shutdown drain, no race.

- `crates/nostos-server/src/main.rs:352` — handle retained (`repl_handle = Some(drv)` at `:412`, pg branch only).
- `crates/nostos-server/src/main.rs:609` — abort inserted between `axum::serve(...).with_graceful_shutdown(...)` and `op_log_shutdown.shutdown().await`.

### Fix B — channel-authority correctness (`crates/nostos-infra/src/oplog.rs`)

`PgOpLogWriter.tx` is now `std::sync::Mutex<Option<mpsc::Sender<OpEntry>>>`. `shutdown()` `.take()`s the sender **first**, before notifying the flush loop and awaiting its handle. This makes the flush loop's all-senders-dropped `rx.recv() => None` arm authoritative: `None` returns only when the receiver queue is empty, so the post-break drain loses nothing. Any concurrent `append()` finds the sender gone (`None`) and rejects loudly via `oplog_dropped` (the same metric + drop path used for a full buffer) — **never silently buffers**.

- `crates/nostos-infra/src/oplog.rs:303` — field type.
- `crates/nostos-infra/src/oplog.rs:354` — `append()` branches on `Some(sender)` vs `None`.
- `crates/nostos-infra/src/oplog.rs:385` — `shutdown()` takes sender first, then notifies + awaits.

### Why both

A alone is **production ordering** — it narrows the race window to zero on the happy path (the replicator task is aborted before the drain runs), but it is a property of one caller (the axum shutdown sequence in `main.rs`). Any other caller of `OpLogWriter::shutdown()`, or any future code that clones the sender across the shutdown boundary, re-opens the race. B alone is **channel-authority correctness for all callers** — even if a sender clone outlives the abort, the authority-layer take means late appends reject loudly. A keeps the producer from racing the consumer; B makes the channel itself the durability authority. The two compose: A is the operational guarantee, B is the invariant the channel enforces regardless of caller discipline.

## Consequences

- **Positive — channel-authority semantics:** the sender is now the authority for "is this writer live." `Some(sender)` → accept (try_send); `None` → reject via `oplog_dropped`. The flush loop's `rx.recv() => None` path becomes the authoritative "buffer is empty, safe to exit" signal. Late appends are loud (counter bump + metric `cairn_oplog_dropped_total`), never silent.
- **Positive — shutdown ordering:** producer-before-consumer is documented at `main.rs:609` and guarded by `repl_handle.take().abort()` at `:615`. The drain at `:623` runs against a stopped producer.
- **Positive — guard test:** the `#[ignore]`'d bug-demo `drain_boundary_late_append_during_final_flush_is_lost` was un-ignored, renamed `drain_boundary_late_append_is_rejected_not_lost` (`oplog.rs:1014`), and its assertion was flipped from `late_result.is_ok()` (demonstrated the loss) to `send_result.is_none()` (guards the rejection). A second test, `post_shutdown_append_is_rejected_as_closed` (`oplog.rs:918`), guards the post-close sub-case. Both PASS today and fail under a regression that keeps the sender alive past `shutdown()`. `make ci` green at 431 passed.
- **Negative — flush_loop post-break path still lacks a second drain** (ponytail: `oplog.rs:1008`). Fix B closes the silent-buffer window by dropping the sender on shutdown, **not** by adding a second drain. If a future change re-introduces a sender clone that outlives `shutdown()`, the `drain_boundary_late_append_is_rejected_not_lost` assertion flips to `Some(_)` and the ghost-row vector re-opens. The guard test is load-bearing.
- **Negative — `std::sync::Mutex` on the sender:** `append()` now locks a std Mutex on every event. The critical section is a `match` on `Option::as_ref()` + `try_send` (no `await` inside) — contention is bounded to the flush task never contending (it owns `rx`, not `tx`). Acceptable while the fan-out loop's per-event cost is dominated by the `try_send` itself; revisit if a benchmark shows the lock.

## Relationship to ADR-0025

Extends slice-6 (`e395dea`). Slice-6 added the shutdown drain; this ADR closes the late-append race the drain exposed. The "reconcile covers it" assumption carried over from slice-1 is **falsified for the matching-epoch path**: slice-1 reconcile (`nostos-core/src/apply.rs:597`, `snapshot_reconcile_removes_orphans_absent_from_snapshot`) is snapshot-only, and ADR-0025 F2's `resume_info{epoch}` skips the snapshot on epoch-match — so a silently-dropped DELETE on that path has no reconcile backstop. F2's snapshot-skip is correct only if every DELETE in the offline gap is durably persisted to `cairn_oplog` and replayed; this ADR is what makes that true under SIGTERM.

## Open follow-ups

- **~~Real-PG `cairn_oplog` INSERT write-amplification is still unmeasured.~~** **CLOSED 2026-08-05.** ADR-0025's "every WAL event also writes a `cairn_oplog` row" consequence is measured by `crates/nostos-infra/tests/e2e_pg_write_amp.rs`: amplification **exactly 1:1** (`amp=1.000`), **`oplog_dropped=0`**, no fan-out regression (the INSERT is off-loop; `RESULTS.md`). This ADR remains correctness-only — no new writes, only ordering + channel-authority.
- **Multi-sender discipline.** Fix B is correct for the single-sender design (the `PgReplicator` task is the only `append` caller). If a future change adds a second `OpLogWriter::append` caller (e.g. a backfill path), the `Mutex<Option<Sender>>` authority still holds — but the `abort()` ordering at `main.rs:615` is producer-specific and would need an analogue for the new caller.

## References

- ADR-0025 (persisted operation-log backfill — slice-6 drain + F2 epoch-skip are the immediate priors).
- `nostos-soundness-audit-2026-07-19` (slot-invalidation P0 — same silent-data-loss category; report at `docs/plans/nostos-soundness-audit-2026-07-19.md`).
- Slice-6 baseline commit: `e395dea` (`feat(sync): graceful oplog writer shutdown drains in-flight batch`).
- Fix A: `crates/nostos-server/src/main.rs:352,412,609,615` (working tree, 2026-07-20).
- Fix B: `crates/nostos-infra/src/oplog.rs:303,354,385,391` (working tree, 2026-07-20).
- Guard tests: `crates/nostos-infra/src/oplog.rs:918` (`post_shutdown_append_is_rejected_as_closed`), `:1014` (`drain_boundary_late_append_is_rejected_not_lost`).
- Reconcile backstop that does **not** cover the matching-epoch path: `crates/nostos-core/src/apply.rs:597`.
