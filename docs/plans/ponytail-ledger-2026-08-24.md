# Ponytail debt ledger — harvested 2026-08-24

Every `ponytail:` marker in the tree at harvest time. Mechanical extract; each entry names its ceiling and upgrade path inline. Review cadence: re-harvest after each major wave, close items whose upgrade path shipped.

## Rust sources (crates/ + sdk/)

```
crates/nostos-infra/examples/e2e_server.rs:330:    // ponytail:fixed sleep instead of a join-with-timeout — a dev fixture can
crates/nostos-infra/src/jwks.rs:36:/// ponytail: fixed, not configurable — no deploy has asked for a different
crates/nostos-infra/src/jwks.rs:127:        // Cache miss or stale: refresh under the write lock. ponytail: this
crates/nostos-infra/src/token_store.rs:63:/// ponytail: single connection; pool when a real load shows contention
crates/nostos-infra/src/token_store.rs:130:    /// ponytail: 30d constant, per-account piggyback on register (no
crates/nostos-infra/src/transport.rs:76:/// verification is per-table and coarse — see the `ponytail:` at the
crates/nostos-infra/src/transport.rs:655:                        // ponytail: swap verification is coarse — ANY per-table
crates/nostos-infra/src/transport.rs:961:    // ponytail: pre-D2 clients get the rules checksum folded into the
crates/nostos-infra/src/push/remote.rs:42://! ## Daemon outage (ponytail: no durable spool in v1)
crates/nostos-infra/src/push/remote.rs:282:        // ponytail: Live Activity delegation is deferred — the 0.2.0
crates/nostos-infra/src/push/fcm.rs:230:                        // ponytail: parts are matched by position (Content-ID
crates/nostos-infra/src/push/fcm.rs:254:    /// ponytail: the lock is held across the token fetch, so concurrent
crates/nostos-infra/src/push/webpush.rs:261:/// too. ponytail: resolve-then-check, not resolve-then-pin — a rebinding DNS
crates/nostos-infra/src/replicator/pg.rs:57:use std::fmt::Write; // ponytail: single write!() in json_escape for a String push
crates/nostos-infra/src/replicator/pg.rs:340:    /// ponytail: a future PG major version adding a new wal_status variant will
crates/nostos-infra/src/replicator/pg.rs:535:    /// initial sync. (ponytail: if a caller later wants a snapshot AT a
crates/nostos-infra/src/replicator/pg.rs:1234:                // ponytail: write! avoids the allocation clippy flags; the trait
crates/nostos-infra/src/replicator/pg.rs:1280:                    // directly (ponytail: if a future version exposes the
crates/nostos-infra/src/replicator/fake.rs:231:        // ponytail: pacing is a per-event sleep, so the real rate is
crates/nostos-infra/src/replicator/typed.rs:26://! ponytail: arrays and typed-decode of domains/enums beyond string
crates/nostos-infra/src/replicator/typed.rs:101:        // (today's behavior). ponytail: see module docs.
crates/nostos-infra/src/replicator/typed.rs:120:// ponytail: affinity mirrors wire reality, not PG semantic type. A future
crates/nostos-infra/src/replicator/typed.rs:285:// fails to parse here and falls back to raw-text passthrough — ponytail:
crates/nostos-infra/src/replicator/snapshot.rs:66:// ponytail: whole-snapshot buffered in memory; stream per-table batches through
crates/nostos-infra/src/replicator/snapshot.rs:113:    // ponytail: whole-snapshot buffered in memory; stream per-table batches through
crates/nostos-infra/src/write_back.rs:180:    use std::fmt::Write as _; // ponytail: single write!() for the $n placeholder
crates/nostos-infra/src/write_back.rs:225:    /// ponytail: pk column fixed to "id"; read from pg_constraint when a
crates/nostos-infra/src/write_back.rs:232:    /// ponytail: single connection; pool when a real load shows contention.
crates/nostos-infra/src/write_back.rs:495:                // ponytail: tenant + OR-set → clobber (no regression vs today; the
crates/nostos-infra/src/write_back.rs:526:            //     ponytail: assumes `tenant.column != PK_COLUMN` (an operator
crates/nostos-infra/src/write_back.rs:567:            //    ponytail: when a schema registry exists (ADR-0012 follow-on),
crates/nostos-infra/src/write_back.rs:1015:            //    is an integer. ponytail: i64 covers every real counter (pomodoro
crates/nostos-infra/src/oplog.rs:34:/// ponytail: tuned constant, no measurement yet; revisit against real-PG
crates/nostos-infra/src/oplog.rs:294:    /// ponytail: single background flush task owns one lazy client (no Mutex —
crates/nostos-infra/src/oplog.rs:422:    /// ponytail: compaction runs on a fixed time-window (`created_at < now() -
crates/nostos-infra/src/oplog.rs:1022:        /// ponytail: the flush_loop's post-break path still lacks a second
crates/nostos-infra/src/snapshot_source.rs:71:/// `PgWriteBack`'s `PK_COLUMN`. ponytail: discover the pk from `pg_constraint`
crates/nostos-infra/src/snapshot_source.rs:80:/// ponytail: per-subscribe `SELECT *` cost + whole-table buffered in memory.
crates/nostos-infra/src/snapshot_source.rs:319:/// ponytail: LSNs share the single u64 space with real WAL LSNs. For a
crates/nostos-infra/src/snapshot_source.rs:334:    // PK column index (v1: the column named "id"). ponytail: read from
crates/nostos-infra/src/wire.rs:14:use std::fmt::Write as _; // ponytail: single write!() in push_json_string
crates/nostos-core/src/in_memory.rs:323:            // ponytail: the two Storage impls diverge here (audit 2026-08-17
crates/nostos-core/src/in_memory.rs:335:        // ponytail: 4b per-principal retention layers above this (ADR-0029
crates/nostos-domain/src/crdt.rs:123:/// user-ids, community tags) are bare strings. ponytail: generalize to a JSON
crates/nostos-client/src/sqlite.rs:93:/// ponytail: `Deserialize` + `pg_oid`/`affinity` arrive when the `GET /schema`
crates/nostos-client/src/sqlite.rs:463:    /// `WHERE col = ?`). ponytail: fast-follow to real typed tables + indexes
crates/nostos-client/src/sqlite.rs:485:            // ponytail: the catalog always has ≥1 column; we don't special-case
crates/nostos-client/src/sqlite.rs:680:                            // is negligible; ponytail: a bulk-merge statement if a
crates/nostos-client/src/sqlite.rs:1275:                // until flush). ponytail: instant-local feedback needs a column
crates/nostos-client/src/sqlite.rs:1284:        // ponytail: 4b per-principal retention layers above this (ADR-0029
crates/nostos-client/src/sqlite.rs:1336:/// `myschema_tasks` (SQLite has no schema-qualified local table here). ponytail:
crates/nostos-client/src/client.rs:164:    /// ponytail: this is a heuristic, not a protocol guarantee. A single
crates/nostos-cli/src/pg.rs:218:/// ponytail: TLS is unverified against a real Supabase project (W0b —
crates/nostos-cli/src/commands/rules.rs:501:        // ponytail: a blank line (including EOF, which read_line also leaves
crates/nostos-cli/src/commands/link.rs:42:// body is pure file IO today. ponytail: drop this allow if link grows a
crates/nostos-cli/src/commands/dev.rs:91:    /// ponytail: this always spawns a *child process*, whichever branch —
crates/nostos-cloud/src/routes.rs:593:        // ponytail: one app instance, full flow — the session cookie from
crates/nostos-server/tests/put_rules.rs:11:/// ponytail: pick a free port here, then hand it to the child via
crates/nostos-server/src/push_api.rs:168:    //    ponytail: check-then-upsert races under concurrent POSTs — worst
crates/nostos-server/src/main.rs:1112:/// renders. ponytail: placeholder coupling; the upgrade is a real
crates/nostos-server/src/main.rs:1617:    // here (ponytail: add `X-Cairn-Source` if the panel needs separating).
crates/nostos-server/src/main.rs:1673:/// ponytail: no optimistic concurrency between the CLI editor and PUT
crates/nostos-application/src/ports.rs:54:    /// not crash metrics rendering — `ponytail:` add a new variant when one
crates/nostos-application/src/ports.rs:641:/// ponytail: `main.rs` wires `PgSnapshotter` unconditionally under
crates/nostos-application/src/ports.rs:839:/// backed by `PgSchemaSource` under `NOSTOS_REPLICATOR=pg`. ponytail: no tenant
crates/nostos-application/src/ports.rs:955:    /// ponytail: unbounded map — one entry per pushed account, so the
crates/nostos-application/src/fanout.rs:379:                // tenants. ponytail: without a tenant column the event
crates/nostos-license/src/lib.rs:112:#[allow(clippy::cast_possible_truncation)] // ponytail: masked value is always 0..=255
crates/nostos-push/src/limit.rs:19://! ponytail: the knobs are process-wide, not per-tenant — one daemon, one
crates/nostos-push/src/config.rs:63:    /// ponytail: 10/sec is a daemon-shape default, not a measurement;
crates/nostos-push/src/config.rs:78:    /// ponytail: 10k is a pinned safe ceiling, not a measurement; upgrade
crates/nostos-push/src/store.rs:530:    /// ponytail: single connection, and the guard is held across each
crates/nostos-push/src/auth.rs:23://! ponytail: CLI key CRUD + hashed-at-rest storage deferred to v1.1 (pin
crates/nostos-push/src/coalescer.rs:31://! ponytail: no retries in v1 — a transient rail outcome is terminal on the
crates/nostos-push/src/coalescer.rs:70:    /// ponytail: 10k open keys / 64 losers per key are daemon-shape guesses
crates/nostos-push/src/api.rs:289:/// ponytail: a priority override seam lands when/if a rail grows one; the
crates/nostos-ffi-wasm/src/transport.rs:18://!   (ponytail: WS glue untested in CI).
crates/nostos-ffi-wasm/src/transport.rs:509:/// for the next open. ponytail: WS glue untested in CI; covered by the manual /
crates/nostos-ffi-wasm/src/transport.rs:538:/// `off_change` / socket `Drop` is the only true end. ponytail: WS glue is
crates/nostos-ffi-wasm/src/transport.rs:561:/// `#[wasm_bindgen] async fn` `NostosSocket::connect`. ponytail: WS glue
crates/nostos-ffi-wasm/src/transport.rs:669:        // ships them. mark_done on send success. ponytail: PendingWrite carries
crates/nostos-ffi-wasm/src/transport.rs:687:        // error". ponytail: no retry/backoff — the E3 demo reloads the page; a
crates/nostos-ffi-wasm/src/transport.rs:738:        // a Close frame (a hard transport error). ponytail: log in production.
crates/nostos-ffi-wasm/src/transport.rs:775:    // the same behavior as the native client's connect. ponytail: no explicit
crates/nostos-ffi-wasm/src/transport.rs:793:/// keeps a text fallback from panicking). ponytail: WS glue untested in CI.
crates/nostos-ffi-wasm/src/lib.rs:341:/// ponytail: a proper UUID would be more collision-resistant but adds a dep;
crates/nostos-ffi-wasm/src/lib.rs:765:    /// ponytail: mirrors SyncClient::or_set_add (client.rs L571); rewire to
crates/nostos-ffi-wasm/src/lib.rs:817:    /// ponytail: mirrors SyncClient::counter_op (client.rs L665); rewire to
crates/nostos-ffi-wasm/src/lib.rs:963://    (ponytail: WS glue untested in CI).
crates/nostos-ffi-wasm/src/lib.rs:1120:    /// outbox id (ponytail: `PendingWrite` — a `nostos-core` domain type —
crates/nostos-ffi-wasm/src/lib.rs:1318:    /// ponytail: the current transport is single-table at the engine level
crates/nostos-ffi-wasm/src/lib.rs:1372:        // db_handle. ponytail: a full in-place reconnect requires making `ws`
crates/nostos-ffi-wasm/src/lib.rs:1649:    //! page manual check — ponytail: browser wasm-bindgen-test setup is
crates/nostos-ffi-wasm/src/sqlite_wasm.rs:361:    /// ponytail: the JS glue (`sqlite_wasm_glue.js`) should expose
crates/nostos-ffi-wasm/src/sqlite_wasm.rs:751:        // ponytail: mirrors SyncClient::write → apply_local; rewire to share
sdk/nostos_tauri/build.rs:4://! ponytail: this is the canonical Tauri-2 plugin build.rs — it does no
sdk/nostos_tauri/src/lib.rs:23://! # ponytail: `unsafe` policy
sdk/nostos_tauri/src/lib.rs:28://! # ponytail: deferred surfaces (upgrade path)
sdk/nostos_flutter/rust/src/api/nostos.rs:46:/// ponytail: `Connected` is a heuristic, not a precise signal from
sdk/nostos_flutter/rust/src/api/nostos.rs:272:    /// ponytail: the transient double-open is a one-time setup cost (cheap);
sdk/nostos_kotlin/src/lib.rs:29://! # ponytail: `unsafe` policy
sdk/nostos_kotlin/src/lib.rs:37://! # ponytail: deferred surfaces (upgrade path)
sdk/nostos_kotlin/src/lib.rs:130:/// The scaffold's module `ponytail:` flagged UniFFI 0.28's **async**-foreign-
sdk/nostos_kotlin/src/lib.rs:162:/// Kotlin's view — see the module `ponytail:` for why we chose sync-over-block
sdk/nostos_kotlin/src/lib.rs:181:    /// ponytail: in-memory only — tokens registered before a process restart
sdk/nostos_kotlin/src/lib.rs:311:    /// # ponytail: poll-only
sdk/nostos_kotlin/src/lib.rs:681:    /// ponytail: a fresh reqwest client per call — registration is a rare
sdk/nostos_dotnet/src/lib.rs:29://! # ponytail: `unsafe` policy
sdk/nostos_dotnet/src/lib.rs:41://! # ponytail: deferred surfaces (upgrade path)
sdk/nostos_dotnet/src/lib.rs:174:/// .NET's view — see the module `ponytail:` for why we chose sync-over-block
sdk/nostos_dotnet/src/lib.rs:195:    /// ponytail: in-memory only — tokens registered before a process restart
sdk/nostos_dotnet/src/lib.rs:763:    /// ponytail: a fresh reqwest client per call — registration is a rare
sdk/nostos_node/src/lib.rs:18://! # ponytail: `unsafe` policy
sdk/nostos_node/src/lib.rs:26://! # ponytail: deferred surfaces (upgrade path)
sdk/nostos_node/src/lib.rs:79:    /// ponytail: in-memory only — tokens registered before a process restart
sdk/nostos_node/src/lib.rs:232:    /// ponytail: no row-tick callback is delivered yet (ThreadsafeFunction
sdk/nostos_node/src/lib.rs:400:    /// ponytail: precision is lost for ids >= 2^53 (napi-rs does not auto-convert
sdk/nostos_node/src/lib.rs:519:    /// ponytail: a fresh reqwest client per call — registration is a rare
sdk/nostos_swift/src/lib.rs:26://! # ponytail: `unsafe` policy
sdk/nostos_swift/src/lib.rs:34://! # ponytail: deferred surfaces (upgrade path)
sdk/nostos_swift/src/lib.rs:136:/// The scaffold's module `ponytail:` previously flagged UniFFI 0.28's
sdk/nostos_swift/src/lib.rs:176:/// Swift's view — see the module `ponytail:` for why we chose sync-over-block
sdk/nostos_swift/src/lib.rs:198:    /// ponytail: in-memory only — tokens registered before a process restart
sdk/nostos_swift/src/lib.rs:324:    /// # ponytail: poll-only
sdk/nostos_swift/src/lib.rs:699:    /// ponytail: a fresh reqwest client per call — registration is a rare
```

## Infra (docker/, compose, SQL)

```
docker/pg-init/02-nostos-role.sql:21:-- ponytail: the password is a throwaway local-Docker dev secret (mirrors the
docker/pg-init/01-sources.sql:30:-- Single-tenant v1: all rows sync (no where_sql partitioning). ponytail:
docker/pg-init/01-sources.sql:63:-- offsets within the day (0..1440). ponytail: dated exception/override slots.
docker/docker-compose.yml:20:      # ponytail: headroom for the real-PG e2e suite — `cargo test -p nostos-infra
```

Total markers:      125
Related: the 2026-08-24 bounded-sink finding adds one new follow-up — see ADR-0040 (Proposed) rather than a code marker.
