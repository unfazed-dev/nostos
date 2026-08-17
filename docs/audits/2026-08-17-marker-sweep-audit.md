# Marker-Sweep Audit — 2026-08-17

**Scope:** all in-code markers in `crates/*/{src,tests,examples}` (target/ excluded): `ponytail:`, `TODO`, `FIXME`, `XXX`, `HACK`, `SAFETY`, `unwrap()`, plus a deadlock/liveness scan of every file combining `spawn` + `Mutex`.
**Method:** ripgrep sweep → 5 parallel audit agents (ponytail × 3 file clusters, unwrap panic-path audit, spawn+Mutex deadlock scan) → every VERIFIED claim independently re-traced by the lead in the source (including the vendored `pgoutput-0.0.7` crate source). Read-only; nothing modified.

**Marker census:** zero `TODO`/`FIXME`/`XXX`/`HACK`/`SAFETY` markers exist in the workspace — `ponytail:` is the only convention (~100 markers, all read in context). ~750 `unwrap()` calls in src files; 747 test-only or provably guarded, 6 production (all safe, see NOISE). 28 files combine spawn+Mutex; no std-guard-across-await and no lock-order inversion exists anywhere.

---

## VERIFIED DEFECTS (23)

### HIGH

#### H1. Cross-tenant data leak via snapshot-on-subscribe — the ports.rs:633 deferral precondition is enforced nowhere
- **Where:** `crates/nostos-application/src/ports.rs:633` (trait ponytail); leak path: `crates/nostos-server/src/main.rs:799-807` → `crates/nostos-infra/src/transport.rs:944-984` → `crates/nostos-infra/src/snapshot_source.rs:197`.
- **Wrong behavior:** the ponytail says "no tenant-predicate scoping in v1 — anonymous / single-tenant only … a multi-tenant deploy must NOT wire a SnapshotSource". But `main.rs:800` wires `PgSnapshotter` unconditionally whenever `NOSTOS_REPLICATOR=pg`, with no check against `sync_auth`/tenant mode (tenant scoping is on by default in supabase-jwt mode: `main.rs:466`, default `NOSTOS_TENANT_COLUMN=org_id` at `main.rs:267`). Live fan-out IS tenant-wrapped (`transport.rs:1249-1292`), but the snapshot path calls `snap.snapshot(&req.table, base_lsn)` with no `TenantScope` and delivers every returned row straight to the sink via `deliver_awaiting` (`transport.rs:963-969`, no predicate filter). `PgSnapshotter`'s SQL is literally `SELECT <cols>::text, … FROM <table>` — no WHERE (`snapshot_source.rs:192-198`).
- **Trigger:** deploy with `NOSTOS_REPLICATOR=pg` + `NOSTOS_SYNC_AUTH=supabase-jwt`. Any client (a) subscribing fresh, or (b) reconnecting with a `resume_lsn` aged out of the op-log window (`transport.rs:930-935` snapshot fallback) receives ALL tenants' rows. Tenant A's device gets tenant B's data.
- **Trace evidence:** trait signature `snapshot(&self, table, base_lsn)` carries no scope (`ports.rs:651-655`); SQL string built at `snapshot_source.rs:197` has no tenant clause; delivery loop `transport.rs:963-969` applies no predicate; wiring at `main.rs:799-807` has no tenant-mode guard (contrast the loud bail for `NOSTOS_PG_URL`-without-pg at `main.rs:829-840`).
- **Severity:** HIGH — silent cross-tenant read leak, reachable by config + one subscribe.
- **Fix direction:** bail at startup when a SnapshotSource is wired with tenant scoping active (mirrors `main.rs:829-840`), or the comment's own upgrade: pass `Option<TenantScope>` into `snapshot`, append `WHERE "<tenant_col>" = $1` bound to the principal's server-stamped tenant value.

#### H2. pg.rs:895 xid clamp — comment's core claim is false; stalls/corrupts client apply on any DB past 2^31 xids
- **Where:** `crates/nostos-infra/src/replicator/pg.rs:894-901`.
- **Wrong behavior:** `self.current_txn = Some(u64::from(u32::try_from(begin.transaction_id.max(0)).unwrap_or(0)))`. The ponytail claims "clamp negatives to 0 (they never occur for real transactions)". Verified false: `pgoutput-0.0.7` decodes the Begin xid wire field (PG `TransactionId` = u32) via `i32::from_be_bytes` (`pgoutput-0.0.7/src/events/message.rs:8,35`). Every xid ≥ 2^31 arrives NEGATIVE — a busy DB (10k txn/s) crosses 2^31 in ~2.5 days; long-lived DBs spend half of every wraparound cycle above it. All such transactions clamp to `txn_id = Some(0)`.
- **Consequence (traced):** in the client apply engine (`crates/nostos-core/src/apply.rs:213-249`) a frame with the same `txn_id` as the open transaction NEVER triggers a mid-transaction flush, and the soft-cap flush is gated on `open_txn.is_none()` (`apply.rs:249`). Consecutive distinct transactions all merge into "txn 0": per-transaction atomicity is silently lost everywhere, and under sustained write load with no quiesce gap (or `flush_quiesce = None`, `client.rs:151`) frames buffer indefinitely — nothing applies, no acks → ack-driven slot advance (ADR-0009) stalls → WAL bloat + unbounded client memory. (The default 50ms `flush_quiesce` masks the stall on idle streams by splitting the mega-"txn" arbitrarily — breaking atomicity instead.)
- **Trigger:** point nostos at any PG whose xid counter ≥ 2^31 (or inject a pgoutput Begin with `transaction_id = -1`) and stream ≥ 2 transactions: the second never closes the first client-side.
- **Severity:** HIGH — real-world reachability grows with DB age.
- **Fix direction:** reinterpret, don't clamp — `begin.transaction_id as u32 as u64`.

### MEDIUM

#### M1. write_back.rs:513 — comment mis-describes the failure direction: tenant.column == PK_COLUMN makes the upsert guard FAIL OPEN
- **Where:** `crates/nostos-infra/src/write_back.rs:513-518` (ponytail); mechanism at `write_back.rs:519, 528-529, 596-617, 652-668`.
- **Wrong behavior:** the comment claims `NOSTOS_TENANT_COLUMN=id` → stamped value filtered out by the `!= PK_COLUMN` guard → "the ON CONFLICT guard's `rows == 0` check could misfire as a false Forbidden" (fail-closed). Verified actual: with `tenant.column == "id"`, `stamp_tenant_column` inserts `"id"`; `columns` then excludes it (L528-529). The guard is still emitted from `t.column` (L607-611): `ON CONFLICT ("id") DO UPDATE SET … WHERE "t"."id" = EXCLUDED."id"`. On a conflict the existing row's id IS `EXCLUDED.id` by definition (id is the conflict target) → the guard is a tautology → update always applies, rows=1 → `Ok(())` with a success ack.
- **Trigger:** operator sets `NOSTOS_TENANT_COLUMN=id`; a principal of tenant A upserts pk `row-1` (owned by tenant B) with payload `{"title":"x"}` → row silently overwritten cross-tenant. ADR-0018 write-path tenant enforcement fully bypassed. (`patch` L937-944 and `delete` L759-766 DO fail closed under the same misconfig — `id = $pk AND id = $tenant` matches nothing — so the comment's false-Forbidden story only fits patch/delete, not upsert.)
- **Severity:** MEDIUM — config-gated security inversion; not attacker-reachable on a correct config, but the safety property inverts silently under a plausible misconfig.
- **Fix direction:** startup validation `tenant.column != PK_COLUMN`.

#### M2. pg.rs:1175 — unchanged-toasted column rendered as "" corrupts the client replica today
- **Where:** `crates/nostos-infra/src/replicator/pg.rs:1175-1183`.
- **Wrong behavior:** `TupleDataColumn::PGUnchangedToastedValue => Some("")` in `tuple_to_json_payload`; the payload flows to client upsert, oplog, and fan-out. pgoutput emits the `u` unchanged-toast marker in the NEW tuple of an UPDATE whenever a toasted column was unmodified — `REPLICA IDENTITY FULL` does not prevent this (FULL only completes the OLD tuple). Every toastable builtin OID maps to `typed`'s quoted-string branch, so the placeholder renders as a real empty string.
- **Trigger:** table with a >~2KB (TOAST-threshold) text/jsonb/bytea column; `UPDATE` touching only a small column → every client receives that column as `""` and clobbers its local copy → silent divergence until the next snapshot-reconcile.
- **Severity:** MEDIUM/LOW — documented deferral, but unlike the other markers this shortcut yields wrong data on a normal reachable input today.
- **Fix direction:** the comment's own upgrade — `REPLICA IDENTITY FULL`, or a distinct wire sentinel the client treats as "unchanged, keep prior value".

#### M3. jwks.rs — JWKS refresh holds the tokio-RwLock WRITE lock across a no-timeout HTTP fetch → total auth stall
- **Where:** `crates/nostos-infra/src/jwks.rs:65` (`reqwest::Client::new()`, NO timeout), `jwks.rs:122-151` (`cache.write().await` held across `self.fetch().await` at L142).
- **Wrong behavior:** while the writer holds the lock, tokio's write-preferring RwLock blocks ALL readers — even verifies that would hit a fresh cache (L111-116) stall. Contrast: the push rails deliberately built a 10s-timeout shared client (`push/mod.rs:94-101`); jwks did not reuse it. The L118-121 ponytail's premise "brief lock contention … cheap (one small HTTP GET)" is unenforceable today.
- **Trigger:** the Supabase JWKS endpoint accepts TCP but stops responding (or a bogus-kid refetch, rate-limited to 5s, coincides with an IdP hang) → one hung fetch wedges every new-connection auth (`auth.rs:143-144` calls verify per WS connect) indefinitely; the server never recovers without restart. `JwksVerifier` is constructed whenever `jwks_url` is configured (`auth.rs:125`) — production-reachable.
- **Severity:** MEDIUM/HIGH-MEDIUM — auth-path liveness; exactly the fail-closed scenario.
- **Fix direction:** use a timeout client (the push/mod.rs template) and/or fetch outside the write lock with single-flight.

#### M4. write_back.rs:291-304 — pool-of-one `guard.take()` checkout defeats "single writer per row" → CRDT lost-update race
- **Where:** `crates/nostos-infra/src/write_back.rs:291-304` (`client()` checks the pooled client OUT via `guard.take()`, leaving the slot `None`; a second concurrent call opens a SECOND connection at L297); false premise in the comments at `write_back.rs:320-321` and `386-387` ("read-modify-write under the pool-of-one connection — single writer per row, no extra locking").
- **Wrong behavior:** `or_set_merge` (L325-383) and `counter_merge` (L389+) are read-modify-write: SELECT current state (L346-357) → merge own delta in Rust (L359) → `INSERT … ON CONFLICT DO UPDATE` (L366-372). With two connections, two concurrent writes to the same row both SELECT the same state S, merge only their own delta, and the last INSERT wins — one element/increment silently and permanently lost. This breaks the ADR-0030 server-side convergence these paths exist to provide. (Note: this corrects an earlier audit verdict that called the merge path "serialized by pool-of-one — noise"; reading `client()` line-by-line showed the checkout semantics.)
- **Trigger:** two clients concurrently OR-set-add / PN-counter-increment the same row on a table in `NOSTOS_OR_SET_COLUMNS`/`NOSTOS_COUNTER_COLUMNS` — the headline CRDT use case (pomodoro community row). Window: the SELECT→INSERT gap.
- **Severity:** MEDIUM — narrow timing window; surviving value is still a valid CRDT state, but the lost update never converges back.
- **Fix direction:** hold the pool guard across the whole RMW, a real per-row lock, or single-flight per (table, pk).

#### M5. ffi-wasm — RefCell borrow held across a JS callback → BorrowMutError → WASM abort
- **Where:** `crates/nostos-ffi-wasm/src/transport.rs:542-549` (`emit_change` holds `slot.borrow()` across `cb.call0()` — the if-let scrutinee `Ref` lives for the whole body, verified line-by-line); reentrant mutators `crates/nostos-ffi-wasm/src/lib.rs:1253` (`onChange`, `borrow_mut`) and `lib.rs:1263` (`offChange`); initial synchronous tick at `lib.rs:1256`.
- **Wrong behavior:** a JS `onChange` callback that synchronously calls back into `offChange()`/`onChange()` hits `borrow_mut()` on the same RefCell → `BorrowMutError` panic. The workspace ships wasm in release with `panic = "abort"` (`Cargo.toml:218-224`) → the whole WASM instance (engine + socket + pump) dies until Worker reload.
- **Trigger:** `sock.onChange(() => { sock.offChange(); })` — fires immediately because registration emits a synchronous initial tick.
- **Severity:** MEDIUM — one-line JS pattern kills the sync engine in-page.
- **Fix direction:** clone the `JsValue`/`Function` out of the borrow before `call0` (drop the Ref first).

#### M6. main.rs — replicator driver death is silent; /healthz stays green
- **Where:** `crates/nostos-server/src/main.rs:601-608` (fake path) and `main.rs:638-649` (pg path) — the fan-out driver task is spawned and `std::mem::forget`'d; `main.rs:1372-1378` (healthz reports session count only).
- **Wrong behavior:** `FanOutService::run` returns only on terminal stream end; the driver task logs at `info!` and exits (or panics with no log — the JoinHandle is never polled). No supervisor/restart. The server keeps accepting `/sync`, serves snapshots, and delivers zero live events forever while the LB sees `{"status":"ok"}`.
- **Trigger:** terminal replicator failure (slot invalidated beyond the L1251 recreate path, unrecoverable stream error) or a fan-out panic.
- **Severity:** MEDIUM — availability + observability; load balancer keeps routing to a zombie.
- **Fix direction:** supervise the driver (restart with backoff or crash the process); fold driver liveness into `/healthz`.

#### M7. transport.rs:756 — read_loop.abort() can land mid-register_subscribe → permanent session/presence leak
- **Where:** `crates/nostos-infra/src/transport.rs:756` (abort); window spans `manager.connect()` (`transport.rs:858-861`) → `subs.ids.push` (`transport.rs:914-918` / `987-992`), including the whole snapshot SELECT (`transport.rs:944-969`); teardown only disconnects ids already in `subs.ids` (`transport.rs:747-753`).
- **Wrong behavior:** if the client disconnects (or a rules-reload closes the socket) while a mid-session Subscribe is mid-flight, the session row stays in `InMemorySessionStore` forever: `live_count` inflated (cap drift), `by_account` never decremented → `account_online` permanently true (`store.rs:258`) so the push router treats an offline account as online, and the zombie stays in `candidates_for`. A narrower variant leaks one cap slot between `try_add_below_cap` (`store.rs:129-137`) and `insert_indexed`.
- **Trigger:** client disconnects mid-Subscribe during a snapshot of a large table.
- **Severity:** MEDIUM — slow resource leak + wrong presence/push targeting.
- **Fix direction:** register the session id before the await-heavy section, or run teardown-compensation on the abort path.

#### M8. Systemic — no-timeout PG connect/execute under guard/await (5+ sites)
- **Where:** `crates/nostos-infra/src/snapshot_source.rs:108-120`, `crates/nostos-infra/src/schema_source.rs:77-89` & `178-190`, `crates/nostos-infra/src/token_store.rs:88-100`, `crates/nostos-infra/src/write_back.rs:292-299` (pool-of-one tokio-Mutex guard held across `tokio_postgres::connect`/statements, no timeout); `crates/nostos-infra/src/oplog.rs:390-403` + `crates/nostos-server/src/main.rs:961-962` (`PgOpLogWriter::shutdown()` awaits the flush task, which may be inside a no-timeout connect/execute at `oplog.rs:679-747`).
- **Wrong behavior:** a blackholed PG (SYN-dropped) wedges all users of that store for the OS TCP timeout (minutes) or forever. Worst amplification: a mid-session Subscribe hangs inside `register_subscribe` (`transport.rs:944-948`) in the socket read_loop, which is also the Ack path → acks freeze → sink backpressure drops live events for that socket. SIGTERM during a PG partition hangs graceful shutdown forever (no timeout wrapper at the `main.rs:961-962` call site).
- **Trigger:** network partition / firewall DROP between nostos and PG during any lazy reconnect.
- **Severity:** MEDIUM — stall, not deadlock (tokio mutexes); same theme as M3. The push rails' 10s-timeout client (`push/mod.rs:96-101`) is the existing template.

### LOW

| # | Location | Wrong behavior | Trigger | Severity |
|---|---|---|---|---|
| L1 | `crates/nostos-ffi-wasm/src/lib.rs:338-350` (`derive_replica_id`) | Two false claims: (a) "mirrors `SyncClientConfig::client_id`" — native uses `Uuid::new_v4()` (`client.rs:223`); (b) "sufficient for wasm (one engine per Worker)" ignores that each tab/Worker is a separate wasm instance whose `COUNTER` restarts at 0 → two tabs built in the same millisecond both get `wasm-<ms>-0`. `replica_id` feeds only PN-counter entries (`counter_apply_delta`, `crdt.rs:424-449`), so colliding replicas lose each other's concurrent increments permanently (per-replica max collapses); OR-set unaffected (HLC has no node id, `crdt.rs:62`). | Browser session restore / duplicate-tab constructing two engines in the same ms on a counter table. | LOW |
| L2 | `crates/nostos-push/src/coalescer.rs:31` | Stale claim "callers retry (the RemoteNotifier of Wave 2 will)". The RemoteNotifier exists and its receipt handler (`crates/nostos-infra/src/push/remote.rs:520-522`) maps a "transient" receipt to `metrics.push_failed` ONLY — no retry, no re-enqueue (the embedded router retries transient up to MAX_ATTEMPTS, `router.rs:649-653`). | `POST /v1/send` whose rail answers 429/5xx in the debounce window → terminal "transient" receipt, push silently dropped, device never doorbelled. Mitigated by doorbell semantics (durable LSN checkpoint reconciles; no data loss, just a missed wake-up). | LOW |
| L3 | `crates/nostos-ffi-wasm/src/lib.rs:1345-1380` | `resume()` docs/ponytail promise "creates a new NostosSocket internally … Returns true if a reconnect was initiated"; the code never creates a socket and never returns `Ok(true)`: open → resend subscribe, `Ok(false)`; closed → `Err` telling the caller to call `connect()`. Fictitious API contract (doc-rot); fail-loud so runtime impact is nil. | Any caller following the documented contract always gets `Err` on a closed socket. | LOW |
| L4 | `crates/nostos-core/src/in_memory.rs:319-325` | Stale rationale: "defer [Patch] until a client issues one — demo + Supabase use upsert/delete". `sqlite.rs:1243-1251` implements optimistic Patch precisely because real clients issue patches (the "patch edits don't render offline" regression). `InMemoryStorage::apply_local` still silently no-ops Patch → the two `Storage` impls diverge. | A client on `InMemoryStorage` (tests/demo/WASM-side) issuing `WriteOp::Patch` offline: edit invisible until server echo; same code on `SqliteStorage` renders instantly. | LOW |
| L5 | `crates/nostos-client/src/client.rs:437, 444, 452` | `checkpoint()`/`epoch()`/`rules_checksum()` do `engine.lock().await.<fn>()` → inline sync SQLite SELECT on the async worker thread, deviating from the crate's own `spawn_blocking` convention (`client.rs:766`). | Single-row point reads; a slow-disk stall or cross-process `SQLITE_BUSY` blocks the runtime worker. | LOW (perf) |
| L6 | `crates/nostos-infra/src/push/router.rs:560-576` | Doc/code mismatch: `flush()` doc claims "(and, at shutdown, every pending)" and the coalesce shutdown arm (L474-476) calls it as "final drain", but `flush` unconditionally filters `p.deadline <= now` (L572-576) → not-yet-due hints silently discarded at shutdown, no send, no metric. Harmless (doorbell + LSN checkpoint). | `notify(hint)` then drop all PushRouter clones within the 2s debounce window. | LOW |
| L7 | `crates/nostos-infra/src/push/remote.rs:104-105, 155-163` | `receipt_loop` leaks per RemoteNotifier instance: dropping the notifier ends the deliver task (channel close) but the receipts task polls for the process lifetime holding Arcs + an HTTP connection. | Re-construction (config reload, tests) leaks one polling task each. Documented deferral; still a real leak. | LOW |
| L8 | `crates/nostos-infra/src/store.rs:90-94, 158-165, 222-229, 239-250` | DashMap shard (std RwLock) guards held across `.await` on per-table tokio mutexes. Verified NO deadlock cycle (strict shard→mutex order; no mutex holder re-enters the map); residual: a blocked shard acquisition pins a worker thread; a queued shard writer blocks readers. Same pattern class the module doc (L22-25) says was fixed for `len()`. | Sustained contention on one shard while a per-table mutex holder awaits PG. | LOW |
| L9 | `crates/nostos-infra/src/transport.rs:548/573/588, 904/968` | Stalled-TCP-client hang: a client that stops reading without closing → writer blocks on send, sink fills, `deliver_awaiting` blocks → socket wedges indefinitely. Backstop exists only for tokens with exp (ADR-0029, L477-499); eviction frees the store entry but socket tasks linger. No heartbeat/idle timeout. | Reader stall on a long-lived /sync socket (dev target). | LOW |
| L10 (latent) | `crates/nostos-infra/src/transport.rs:649, 745` | write_loop panic skips `closed_tx.notify_waiters()` → `run_session` hangs forever at `closed.notified().await`, leaking the reader + session registrations. No concrete panic site exists today (encode is infallible) — fragility only. | Any future panic in the write loop. | LOW (latent) |
| L11 (latent) | `crates/nostos-infra/src/transport.rs:609-612` | `continue` on `rules_rx.changed()` Err = 100%-CPU busy-spin: once the watch sender is dropped, `changed()` resolves Err immediately forever → permanently-ready select arm spins the write loop. | Reachable only via `with_rules` miswiring (the exact mistake the L246-252 doc warns about) or shutdown. | LOW (latent) |
| L12 | `crates/nostos-bench/src/bin/bench_pg_ingest.rs:347-348, 363` vs doc L40-42 | Cleanup not "guaranteed on every exit path": two `?`-bails (`from_url`, `wait_slot_active` 15s timeout) precede the teardown block → `bench_pgi_*` replication slot + seed rows leak on the shared dev PG (the slot retains WAL). Slot name is logged (L344); manual recovery possible. | Failed/interrupted bench run against the dev PG. | LOW |
| L13 | `crates/nostos-server/src/main.rs:943-946` | `?` on `axum::serve` skips the shutdown tail: on serve error, replicator abort + op-log drain (L961-962) + rules-watcher stop are skipped; last ≤BATCH_MAX op-log entries dropped instead of flushed (correctness preserved via documented snapshot-reconcile). | `axum::serve` returning an error (bind race, listener death). | LOW |

---

## NOISE — markers verified fine (code matches comment)

### Ponytail markers (all faithful documented deferrals)

**nostos-infra:**
- `wire.rs:14`, `replicator/pg.rs:57`, `write_back.rs:180` — import-site notes for single `write!()` uses.
- `snapshot_source.rs:70, 182` — `PK_COLUMN='id'` convention, consistent with `PgWriteBack`.
- `snapshot_source.rs:79` — per-subscribe `SELECT *` + in-memory buffer; perf ceiling, upgrade path named.
- `snapshot_source.rs:216` — synthetic-LSN collision window. Trigger IS real (`transport.rs:394` seeds `synthetic_cursor` from `resume_lsn`), but harm needs an exact LSN coincidence between a snapshot row (N+1+i) and a live event's wal_end; documented with upgrade path. Borderline — noted, not a defect.
- `oplog.rs:34` — unmeasured `BATCH_MAX=500` tuning constant.
- `oplog.rs:294` — single flush task / pool-of-one; matches code.
- `oplog.rs:422` — time-window compaction; explicitly documented-closed (ADR-0025 F3); correctness argument checks out (slice-1 reconcile floor).
- `oplog.rs:1022` — VERIFIED accurate: `append()` try_sends under the std Mutex (L365-368) and `shutdown()` `.take()`s the sender under the same mutex before notify (L396), so every successful send happens-before the post-break drain; the missing second drain is genuinely covered by fix B, exactly as claimed. Guarded by test `drain_boundary_late_append_is_rejected_not_lost`.
- `replicator/snapshot.rs:66, 113` — whole-snapshot-in-memory; perf ceiling only.
- `replicator/pg.rs:340` — unknown future wal_status → Healthy; future-PG concern, mitigations present.
- `replicator/pg.rs:535` — snapshot-at-explicit-LSN vs start_lsn mutual exclusion; design deferral.
- `replicator/pg.rs:1205` — `write!`-vs-alloc note; `json_escape_into` correct.
- `replicator/pg.rs:1251` — string-match for SQLSTATE 55000; the four substring patterns cover realistic error text; behavior (recreate + resnapshot + alert) correct.
- `replicator/typed.rs:26, 101, 120, 285` — string passthrough for enums/arrays/domains, affinity mirroring, no BC/non-ISO-DateStyle; fallbacks verified graceful (quoted string, never panic/drop; `NaN`/`Infinity`/`-Infinity` correctly quoted per RFC 8259 at L221-228).
- `replicator/fake.rs:231` — per-event sleep pacing cap; eval-only replicator, accurate.
- `jwks.rs:36` — fixed 5s `MIN_REFETCH_INTERVAL` DoS guard; enforced at L130-139.
- `jwks.rs:118` — refresh serialized under write lock with double-check; the comment text matches the code (L122-151). (The underlying no-timeout hazard the comment hand-waves is tracked separately as defect M3.)
- `transport.rs:75, 623` — VERIFIED fail-safe: `decide()` returns `RuleDecision` whose `Allow` wraps the full `PredicateExpr` with derived `PartialEq` (`rules.rs:34-46`), so ANY predicate change on a subscribed table compares unequal → socket closed → client reconnects and re-scopes. No missed verification; coarseness only over-closes (widens included). No silent divergence possible on this path.
- `transport.rs:878` — VERIFIED accurate: `resume_advertisement` (L808-818) folds the rules checksum into the advertised epoch via `compose_sync_epoch` for pre-D2 clients; log-attribution-only ceiling as claimed.
- `write_back.rs:226, 233` — `PK_COLUMN` deferral; pool-of-one. Match code.
- `write_back.rs:482` — tenant+OR-set falls through to clobber; documented scope limitation, behavior as described.
- `write_back.rs:554` — bind-by-inference; mitigated by prepare-then-coerce (`coerce_params` L637-647); accurate.
- `write_back.rs:1002` — i64-only counter delta; `as_i64` rejects fractional input loudly (L1018-1023); matches claim.
- `token_store.rs:64` — pool-of-one for rare push-registration REST calls.
- `token_store.rs:135` — 30d sibling-token sweep on register; SQL at L148-163 matches the comment (per-account, tenant-gated).
- `push/webpush.rs:261` — resolve-then-check SSRF guard (DNS-rebind TOCTOU acknowledged); fails closed on unresolvable/empty/private (L264-294).
- `push/remote.rs:42` — no durable push spool; drop-and-count deliberate (LSN checkpoint is correctness); accurate.
- `push/remote.rs:282` — Live Activity delegation deferred; tokens skipped, not mis-sent (L287-289).
- `push/fcm.rs:230` — batch parts matched by position; count mismatch → all-Fatal, conservative, never misattributed (L229-242).
- `push/fcm.rs:254` — token-cache lock held across fetch; NOT a defect: the shared http_client has a 10s timeout (`push/mod.rs:96-101`) bounding any hang, and waiters re-check the cache after acquiring (L259-263) — already effective single-flight. Ponytail accurate.

**nostos-client / nostos-core / nostos-domain / nostos-application:**
- `sqlite.rs:93` — `Deserialize`/`pg_oid` deferred until `GET /schema` (WS3) lands; name/PK/columns suffice for the JSON1 view path.
- `sqlite.rs:463` — view-per-table full-scan ceiling on `WHERE col=?`; perf-only, accurately described.
- `sqlite.rs:485` — empty-column-list claim's parenthetical slightly off (`pk AS _pk` is always led with, so DDL stays valid anyway); behavior safe either way.
- `sqlite.rs:680` — OR-set per-row read-merge-write; perf ceiling, LSN-gated, degrades to LWW as stated.
- `sqlite.rs:1275` — Increment server-authoritative (ADR-0030 D1); intent durable in outbox, echo reconciles. Correct.
- `sqlite.rs:1284` — sign-out `clear()` discards all pending writes; loses unsynced work but preserves cross-user isolation, exactly as documented (ADR-0029 §4b upgrade named).
- `sqlite.rs:1336` — `view_name` `a.b`→`a_b` collision needs a bare `a_b` table also synced; narrow, risk + upgrade named.
- `client.rs:137` — flush_quiesce heuristic; failure mode documented precisely (mid-txn force-flush → transient half-applied read, no loss/dup); self-consistent.
- `in_memory.rs:331` — same sign-out wipe as sqlite.rs:1284; complete for in-memory (no DLQ state).
- `fanout.rs:341` — bare-account-id presence key → cross-tenant doorbell over-suppression only; cosmetic, as stated.
- `fanout.rs:379` — VERIFIED by tracing: tenants derive only from (a) the row's server-stamped tenant column (L385-395; invariant: same column write_back force-stamps) or (b) matched sessions' own tenants (L397-403) → NO wrong-tenant delivery, NO leak; the gap is a missed best-effort doorbell with the durable LSN checkpoint as correctness mechanism. Accurate.
- `ports.rs:54` — `SlotHealth::from_u8` unknown→Healthy; metrics-rendering-only, cosmetic.
- `ports.rs:789` — `GET /schema` unscoped exposes publication-wide metadata only (no rows); low sensitivity, accurate. (The same "row isolation is the read-path's job" argument FAILS for L633 because snapshots carry rows — see defect H1.)
- `ports.rs:905` — `push_last_lsn` unbounded map: keyspace verified = server-authenticated `Principal::account_id`s (`fanout.rs:276-285` → `router.rs:620`) or echoes of server-originated hint metadata (`remote.rs:316`); not client-inflatable; growth = distinct pushed accounts, exactly the named ceiling.
- `crdt.rs:123` — type-generalization deferral (bare-string element values → JSON); no correctness claim. Confirmed noise by direct read (`crdt.rs:115-134`).

**nostos-ffi-wasm / nostos-server / nostos-push / nostos-cli / nostos-cloud / nostos-license:**
- `transport.rs:18, 509, 553, 785`, `lib.rs:963`, `lib.rs:1650`, `Cargo.toml:58` — "WS glue untested in CI" / env-flaky wasm-bindgen-test; true, pure layer host-tested.
- `transport.rs:538` — emit_change fire-and-forget glue.
- `transport.rs:661` + `lib.rs:1120` — `PendingWrite` has no `client_write_id`; `flush_pending` synthesizes the wire id from the outbox id (confirmed L522-528); correlation-id loss across offline flush, consistent both sides.
- `transport.rs:679` — no retry/backoff; closes 1011 as described.
- `transport.rs:730` — "log in production" note.
- `transport.rs:767` — no connect deadline; browser handshake timeout covers it.
- `lib.rs:765, 817` — wasm mirrors native `or_set_op`/`counter_op` (same `counter_apply_delta`+replica pattern as `client.rs:683`).
- `lib.rs:1318` — multi-table subscribe shares one global resume checkpoint; native does the same (`client.rs:1087`), so wasm has parity.
- `sqlite_wasm.rs:361` — selectObjects→selectRows fallback; shape difference self-documented.
- `sqlite_wasm.rs:751` — `apply_local` mirror of native.
- `main.rs:1060` — liveactivity placeholder PushTemplate coupling; router consults live_activities first as stated.
- `main.rs:1547` — audit source always "api"; `X-Cairn-Source` upgrade path.
- `main.rs:1603` — last-write-wins accurate; CLI-vs-PUT lost update reachable exactly as documented (`apply_put_rules` snapshots 'hand' at L1641, writes at L1670). One undocumented wrinkle: two concurrent PUTs can interleave save/swap so memory briefly enforces the LOSER, but `watch_rules` (L1014) dedupes against `rules_tx.borrow()`, so the next poll tick self-heals. Transient only.
- `push_api.rs:168` — race claim accurate: upsert is `INSERT..ON CONFLICT(token) DO UPDATE` (`token_store.rs:152`) keyed by token → no dup rows; over-cap bounded by in-flight concurrency; "a few rows over cap, never unbounded" holds.
- `coalescer.rs:66` — 10k/64 ceilings are env-exposed guesses.
- `api.rs:289` — priority validated then dropped; rails derive priority (no override knob exists).
- `auth.rs:23` — env-seeded keys, CRUD deferred; constant-time SHA-256 compare present.
- `store.rs:527` — `PgStore` pool-of-one, mutex held across statements; matches the SQLite twin.
- `config.rs:63, 78` — guessed rate/ceiling defaults, env-exposed.
- `limit.rs:19` — process-wide token-bucket knobs; code matches.
- `dev.rs:91` — always child-process spawn; no in-process embed.
- `rules.rs:500` — blank/EOF line defaults to 'q'; prevents /dev/null spin.
- `link.rs:42` — `unused_async` allow justified; body is pure file IO.
- `cli/pg.rs:218` — `connect_tls` unverified against real Supabase; standard rustls recipe; honest.
- `cloud/routes.rs:593` — test-scope note (one app instance, full flow).
- `license/lib.rs:112` — "masked value always 0..=255" verified correct (buf holds exactly bits+8 bits at push; bits<8 by post-mask invariant).
- `tests/put_rules.rs:11` — free-port TOCTOU in tests; failure is flaky connection-refused, not false pass.

### unwrap() audit (all ~750 src-file occurrences)

**VERIFIED reachable-panic defects: NONE.** Method: per-file `#[cfg(test)]` boundary analysis cross-validated by an awk brace-depth tracker classifying every unwrap as inside/outside a cfg(test) scope; both methods agree exactly. Of ~750 calls, exactly 6 are production code, all safe:

1. `crates/nostos-domain/src/predicate_compile.rs:254` — `parts.pop().unwrap()` guarded by `parts.len()==1`; vec always non-empty (first pushed unconditionally). Parser reachable by external predicate strings, but this unwrap cannot fire.
2. `crates/nostos-domain/src/predicate_compile.rs:269` — same pattern in `parse_and`.
3. `crates/nostos-infra/src/oplog.rs:365` — std `Mutex::lock().unwrap()` in `PgOpLogWriter::append`; the only code under the guard is `build_entry` (pure clones; its `i64::try_from(lsn).expect` cannot fire for real PG LSNs ~2^40 — would need ≥2^63 bytes of WAL) + `try_send`. Poisoning unreachable.
4. `crates/nostos-infra/src/oplog.rs:396` — `lock().unwrap().take()` in `shutdown()`; nothing panicking under the guard.
5. `crates/nostos-cloud/src/routes.rs:229` — `serde_json::to_value(&proj).unwrap()`; derived-Serialize struct of String/Option<String>/i64 — infallible (no non-string map keys).
6. `crates/nostos-cloud/src/routes.rs:254` — same for `ApiKey`.

Everything else is test-only: all 170 in `sqlite.rs` sit inside `#[cfg(test)] mod tests` (line 1438+); `main.rs`'s 24 are spread across 8 inline `#[cfg(test)] mod *_tests` blocks; no top-level production item follows the first tests mod in any of the 33 audited files. Adjacent theoretical note (expect, not unwrap): `oplog.rs:68` `i64::try_from(lsn).expect` panics only at LSN ≥ 2^63 — unreachable for real Postgres.

### spawn+Mutex deadlock scan (28 files, read end-to-end)

- No std-guard-across-await anywhere; no lock-order inversion anywhere; no orphaned-waiter hang found.
- `oplog.rs:308` std `Mutex<Option<Sender>>` — try_send only under guard, no await; senders can NEVER block; a dead flush task → channel closed → try_send Err → counted as `oplog_dropped`. NOISE for the sender side (the real oplog hazard is the shutdown-await hang, defect M8).
- `oplog.rs:127` RecordingOpLogWriter detached drain — exits on sender drop; drops counted. Fine.
- oplog compactor — detached, reconnect-on-error, no waiter. Fine.
- application fanout/ports/session — zero production locks; test-double guards all statement-scoped; JoinSet fully drained; push-hint drain degrades to counted drops. Clean.
- `ports.rs:908` Metrics::push_last_lsn std Mutex — guard in one sync block, poison-tolerant. Fine.
- `push/router.rs:158` InMemoryTokenRegistry std Mutex — all 6 async methods confine guards to single sync statements. Fine.
- push coalescer/api — try_send + release-on-failure, no gate-slot leak on cancel; `PgStore::with_client` lock-across-await is the documented pool-of-one, bounded by 15s connect + 30s statement timeout. Fine.
- `client.rs` token/hlc std locks — sync-only; engine tokio Mutex never across await; `blocking_lock` inside `spawn_blocking` correct. `sqlite.rs` conn Mutex — `apply_batch` takes the lock AFTER `pending()` (documented anti-re-entrancy, L633-637). Clean.
- `router.rs` (infra):83 std `Mutex<DedupRing>` — sync-only; poison-swallow degrades dedup but apply is idempotent. Fine.
- server main `rules_shared` tokio RwLock — guards are statement temporaries, never nested. Fine.
- write_back conn-driver spawn — dead conn fails requests with errors, evicted via `drop_client`; no hung waiter. Fine.
- store.rs pool take()/overwrite churn — wasteful, never incorrect; matches the documented ponytail. Fine.
- `nostos-push/store.rs` + `nostos-cloud/store.rs` `Arc<tokio::Mutex<Connection>>` — only sync rusqlite after lock; guards dropped at fn end. Fine.
- `limit.rs` std Mutex — fully synchronous. Fine.
- `outbox.rs`/`storage.rs` — pure sync traits, zero locks. Fine.
- bench main.rs Histogram mutexes — never across await; all waits bounded (timeouts + abort). Fine.

---

## THEMES

1. **Missing network timeouts on PG/HTTP calls** (jwks, snapshot_source, schema_source, token_store, write_back, oplog flush) interacting with pool-of-one guards and shutdown awaits — the systemic liveness weakness. The push rails' 10s-timeout client (`push/mod.rs:96-101`) is the existing fix template.
2. **Task-teardown gaps** — abort/panic windows (`register_subscribe` race, write_loop `notify_waiters` skip) and unsupervised driver death with a green `/healthz`.
3. **Ponytail discipline is otherwise excellent** — ~90 of ~100 markers are faithful documentation; the defects cluster where a deferral's *safety precondition* is not enforced by the composition root (H1, M1) or where the comment's factual claim about an external format is wrong (H2, M2).

## TOP ACTIONS (priority order)

1. **H1** — bail at startup when a SnapshotSource is wired with tenant scoping active (mirror `main.rs:829-840`), or thread `Option<TenantScope>` into `snapshot()` per the comment's own upgrade.
2. **H2** — one-line fix at `pg.rs:895`: `begin.transaction_id as u32 as u64`.
3. **M1** — startup validation `tenant.column != PK_COLUMN`.
4. **M3 + M8** — workspace-wide pass: every `reqwest::Client::new()` / `tokio_postgres::connect` gets a bounded timeout (reuse the `push/mod.rs` template).
5. **M4** — hold the write_back pool guard across the CRDT read-modify-write (or single-flight per (table, pk)).
6. **M5** — clone the callback out of the RefCell borrow before `call0` in `emit_change`.
7. **M6** — supervise the replicator driver; fold its liveness into `/healthz`.

---

## RESOLUTION (same day, orchestrator-verified fixes)

| Finding | Status |
|---|---|
| H1 cross-tenant snapshot leak | **FIXED** — SnapshotSource::snapshot takes Option<TenantScope> (Principal::tenant_scope seam); PgSnapshotter appends WHERE "col"::text = $1 bound; PG-gated regression e2e_pg_snapshot_tenant_scope.rs; ADR-0011 addendum |
| H2 xid≥2³¹ clamp | **FIXED** — xid_of() reinterprets via cast_unsigned; unit test pins -1→u32::MAX |
| M1 tenant=id fail-open | **FIXED** — loud startup bail in main.rs |
| M2 toasted-column clobber | **FIXED** — old-tuple substitution under REPLICA IDENTITY FULL; unit test; "" fallback retained + documented for non-FULL tables |
| M3 JWKS no-timeout under write lock | **FIXED (bounded)** — 10s/5s timeout client; single-flight fetch-outside-lock remains the ponytail upgrade |
| M4 CRDT RMW lost-update | **FIXED** — pool guard held across the whole RMW in or_set_merge + counter_merge |
| M5 wasm RefCell across JS callback | **FIXED** — callback cloned out, borrow dropped before call0 |
| M6 silent driver death | **FIXED** — driver_dead flag flipped on driver exit; /healthz answers 503 degraded (panic path stays panic-hook-loud, documented) |
| M7 abort-mid-subscribe session leak | DEFERRED — register-before-await is ordering surgery on the session core; not rushed. Leak is slow (disconnect-during-snapshot) and visible via session_count |
| M8 no-timeout PG class | **FIXED** — shared pg_connect_bounded (15s connect + 30s statement_timeout) across snapshot/schema/token/write_back; oplog shutdown wrapped at 35s |
| L1 wasm replica_id collision | DEFERRED (LOW; needs a randomness-source decision on wasm32) |
| L2 coalescer stale retry claim | **FIXED** (comment) |
| L3 wasm resume() doc-rot | **FIXED** (doc) |
| L4 InMemoryStorage Patch divergence | **FIXED** (comment now honest; typed-JSON merge deferred) |
| L5 inline SQLite on async worker | DEFERRED (perf LOW; single-row point reads) |
| L6 flush() doc mismatch | **FIXED** (doc) |
| L7 receipt_loop task leak | DEFERRED (reconstruction-only leak; documented) |
| L8 DashMap shard across await | DEFERRED (verified no deadlock; contention residual only) |
| L9 stalled-TCP hang | DEFERRED (needs heartbeat/idle-timeout design) |
| L10 write_loop panic skips notify | DEFERRED (latent; no panic site exists) |
| L11 rules_rx busy-spin | **FIXED** — break, not continue |
| L12 bench slot cleanup lie | **FIXED** (earlier same day, before this audit read it) |
| L13 serve-error skips shutdown tail | DEFERRED (correctness preserved via snapshot-reconcile) |

Related same-day fixes outside this sweep (docs-audit + logic-review agents):
non-public-schema view_name mismatch (ADR-0028 addendum + view_name_test.dart),
batch-send phase-1 token refund + effective-ceiling 400 (contract YAML updated),
bench_pg_ingest trailing-sleep/slot-leak/contamination-subtraction, apply_bench
router cross-check now actually performed, QUICKSTART known-gaps staleness,
plans/README reconnect-glitch row.
