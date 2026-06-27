# ADR-0016: Client SDK + durable checkpoint + WAL-bloat protection

- **Status:** Shipped (client core + durable checkpoint + WAL-bloat protection); FFI bridges remain (ADR-0015)
- **Date:** 2026-06-27 (deferred) · 2026-06-28 (shipped)

## Context

Three distinct gaps were grouped here because each is a foundation that the
remaining fronts (ADR-0012–0015) build on. As of Tier 2, all three ship:

1. **The client apply engine didn't exist.** The server sent frames; nothing
   received and applied them except the benchmark's WS swarm (an `AtomicU64`
   counter — no storage, no apply, no checkpoint). Tier 0+1 proved no-loss/
   no-duplication *on the wire*; the apply layer was unproven.
2. **Durable checkpoints weren't persisted.** ADR-0009 made resume *correct*
   (ack-driven slot advance), but the client's last-applied LSN lived only in
   memory — a client restart lost it.
3. **No WAL-bloat protection.** ADR-0009's ack-driven model means a
   permanently-silent client keeps the slot from advancing → unbounded WAL
   retention on the customer's primary Postgres.

## Decision

**Ship all three as real code (no stubs).** The [Architecture advisor
(GLM-5.2, HIGH confidence)] chose this front over the dynamic predicate engine
(ADR-0012) because the predicate moat is *untestable in isolation* — nothing
applied frames to durable storage. The client core unblocks the FFI bridges
AND makes the moat measurable, and it closes the Phase 1 kill criterion ("one
real client, end-to-end — no loss, no duplication") that Tier 0+1 only half-met.

[Architecture advisor (GLM-5.2, HIGH confidence)]: # "consulted 2026-06-28 on Tier 2 sequencing"

### What shipped

**`crates/nostos-core`** — the platform-agnostic client sync engine. Pure Rust,
no tokio, no SQLite (WASM-clean — this is what the FFI bridges will bind):

- **`Storage` trait** — two methods: `checkpoint()` and `apply_batch(ops,
  checkpoint)`. The correctness property — *the row writes and the LSN
  checkpoint land in one atomic transaction* — collapses into `apply_batch`, so
  it's structural rather than conventional. (Cut from the advisor's sketched
  five methods to two for ponytail; batching lives on the caller.)
- **`ApplyEngine`** — the apply state machine: buffers frames to a transaction
  boundary (`txn_id` change) or a soft cap (256), then one atomic commit.
  Idempotent by pk (implicit LWW — ADR-0014 tier (a)).
- **`InMemoryStorage`** — the test double + contract reference.

**`crates/nostos-client`** — the native client (where async + rusqlite live):

- **`SqliteStorage`** — real `rusqlite` persistence. Opaque payload bytes per
  `(table, pk)` + an LSN checkpoint in `cairn_meta`. `apply_batch` wraps every
  row op + the checkpoint write in ONE transaction (crash-safe by design).
  **Opaque bytes** is a deliberate scoping: the wire delivers the tuple image
  as hex; a column decoder arrives with ADR-0012. Storage is durable +
  resumable; not SQL-queryable until then.
- **`SyncClient`** — the tokio orchestrator: connect, `Subscribe` with the
  durable `resume_lsn`, drive `ApplyEngine` over the inbound stream via
  `spawn_blocking`, `Ack` each commit, reconnect-with-backoff. `idle_timeout`
  for "sync-then-disconnect" clients.

**WAL-bloat protection** (server-side, `nostos-application` + `nostos-server`):

- **`EvictionPolicy`** — pure logic: evict the slowest session when
  `head - slowest_acked > max_lag`. **OFF by default** (zero behavior change);
  opt-in via `NOSTOS_SLOT_MAX_LAG`. Targets the single slowest session via
  `SessionStore::slowest_session`.
- **`max_slot_wal_keep_size_mb`** — the database-level backstop, set via
  `ALTER_REPLICATION_SLOT` (Postgres 13+); config knob
  `NOSTOS_PG_SLOT_WAL_KEEP_SIZE`. The eviction policy is the first line of
  defense; this is the last resort if a client vanishes entirely.

### What remains deferred (ADR-0015)

The FFI bridges (`flutter_rust_bridge`, UniFFI, `wasm-bindgen`, `napi-rs`) are
NOT in this increment — they bind `nostos-core`, which now exists, so the
prerequisite is met. They remain Phase 2–3 per ADR-0015.

## Consequences

**Positive:** Nostos is no longer server-only. A Rust client (`nostos-client`)
applies frames to durable SQLite and survives disconnect+reconnect with zero
loss and zero duplication — the Phase 1 kill criterion, genuinely met
end-to-end. The FFI bridges and write-back (ADR-0013) now have a real client to
build on.

**Negative:** the stored data is opaque bytes until ADR-0012's column decoder
ships — durable and resumable, but not SQL-queryable. The FFI bridges
(ADR-0015) are still unbuilt, so Flutter/RN/Web/RN clients don't exist yet.

**Kill criterion (WAL-bloat):** met — eviction ships OFF-by-default with a
documented opt-in, and `max_slot_wal_keep_size` is configurable. A deploy MUST
set one (or both) before production; an unbounded slot on a customer's primary
is unacceptable, and both knobs now exist.

## Validation

- **Unit:** 18 `nostos-core` + 13 `nostos-client` + 10 eviction tests.
- **Chaos e2e** (`chaos_resume.rs`): a real `SyncClient` over a real socket
  applies frames, disconnects, reconnects with `resume_lsn` — asserts exactly
  the sent rows, none lost, none duplicated.
- **Throughput microbench** (`throughput.rs`): `SqliteStorage` sustains
  ~370k–440k rows/sec at 1k/10k/100k frames (SQLite is NOT the bottleneck); a
  batched-vs-per-row guard catches any future transaction split.

## Alternatives considered

- **Build the predicate engine (ADR-0012) first:** rejected by the advisor —
  nothing applied frames, so the moat would be tuned against a bench swarm that
  doesn't represent real client consumption.
- **Stub the client SDK:** rejected — a client that doesn't durably apply is
  worse than no client (it would lose data on restart).
- **Preemptive slot advance (ignore acks) to avoid bloat:** rejected — that's
  the original ADR-0009 bug (silent data loss). Correctness before disk.

## References

- Depends on: ADR-0009 (resume), ADR-0010 (auth), ADR-0011 (enforcement).
- Enables: ADR-0013 (write-back needs a client), ADR-0015 (bridges bind this).
- Code: `crates/nostos-core` (engine + `Storage`), `crates/nostos-client`
  (`SqliteStorage` + `SyncClient`), `crates/nostos-application/src/eviction.rs`.
