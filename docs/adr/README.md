# Architecture Decision Records

One file per decision, `NNNN-<slug>.md`, numbered in order; the next free
number is the next ADR. Code cites the ADR it implements. Status below is
condensed from each ADR's own header — the ADR is the source of truth.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-hexagonal-ddd-architecture.md) | Hexagonal (Ports & Adapters) + DDD layering | Accepted |
| [0002](0002-rust-tokio-axum-stack.md) | Rust + tokio + axum for the server | Accepted |
| [0003](0003-dynamic-predicates-not-buckets.md) | Dynamic reactive sync (predicates), not static buckets | Accepted |
| [0004](0004-server-authoritative-lww-conflict-resolution.md) | Server-authoritative LWW default, opt-in CRDT-per-field, custom merge | Accepted — LWW default; CRDT-per-field shipped (ADR-0030); custom merge Phase 4 |
| [0005](0005-apache-2.0-license.md) | Apache-2.0 license, end to end | Accepted |
| [0006](0006-open-core-monetization-via-cloud-and-enterprise.md) | Open-core, monetize via Cloud + Enterprise (the Postgres/Supabase play) | Accepted |
| [0007](0007-platform-assembly-supabase-rust-cloudflare-fly.md) | Platform assembly — Supabase + Rust + Cloudflare + Fly.io | Accepted |
| [0008](0008-visual-identity-the-nostos-field.md) | Visual identity — The Nostos Field | Accepted (founder-approved) |
| [0009](0009-ack-driven-lsn-resume-and-exactly-once.md) | Ack-driven LSN resume and exactly-once delivery | Accepted (shipped) |
| [0010](0010-sync-authentication-and-principal.md) | /sync authentication and the Principal type | Accepted (shipped) |
| [0011](0011-server-enforced-predicates.md) | Server-enforced predicates (never client-attested) | Accepted (shipped) |
| [0012](0012-dynamic-predicate-expression-engine.md) | Dynamic predicate expression engine (Front 1 — the marketed moat) | Moat complete — shipped |
| [0013](0013-direct-write-back-design.md) | Direct write-back (Front 2 — deferred) | Accepted (shipped) |
| [0014](0014-tiered-conflict-resolution.md) | Tiered conflict resolution (Front 6 — LWW shipped, CRDT/custom deferred) | Partially shipped — LWW + CRDT add-wins; custom merge deferred (Phase 4) |
| [0015](0015-ffi-bridge-strategy.md) | FFI bridge strategy (Front 5) | All bridges shipped |
| [0016](0016-client-sdk-and-wal-bloat-protection.md) | Client SDK + durable checkpoint + WAL-bloat protection | Shipped |
| [0017](0017-web-persistence.md) | Web persistence (Front 5 — browser-durable storage) | Follow-up shipped (ADR-0033) |
| [0018](0018-write-path-tenant-enforcement.md) | Write-path tenant enforcement (extends ADR-0011 to writes) | Accepted (shipped) |
| [0019](0019-typed-payload-mapping.md) | Typed payload mapping in `PgReplicator` (server-side, OID-keyed) | Accepted (shipped) |
| [0020](0020-react-native-turbomodule-over-uniffi.md) | React Native SDK via Turbo Native Module over UniFFI (not the WASM JS core) | Accepted (shipped) |
| [0021](0021-client-schema-discovery-rest.md) | Client schema discovery via REST (`GET /schema`) | Accepted (shipped) |
| [0022](0022-flutter-multitable-sync-and-pause-resume.md) | Flutter multi-table sync per handle and real pause/resume | Accepted |
| [0023](0023-dot-nostos-project-directory-and-backend-adapters.md) | The `.nostos/` project directory and pluggable backend adapters | Accepted (shipped) |
| [0024](0024-client-reactive-facade-and-query-primitive.md) | Client reactive facade (`Collection<T>` + `NostosStore`) over the existing hot-replay stream | Accepted (shipped) |
| [0025](0025-persisted-oplog-backfill-for-reconnect-resume.md) | Persisted operation-log backfill for reconnect resume | Accepted |
| [0026](0026-oplog-shutdown-durability.md) | Channel-authority + producer-before-consumer shutdown for op-log durability | Accepted |
| [0027](0027-write-outcome-visibility-in-the-client-sdk.md) | Write-outcome visibility — dead-letter-only error surfacing | Accepted |
| [0028](0028-client-read-views-over-opaque-payload.md) | Client read model is SQLite VIEWs over the opaque payload; materialized typed tables are rejected | Accepted |
| [0029](0029-sign-out-and-local-state-wipe.md) | Sign-out and local-state wipe | Accepted — Decisions 1/3/4 shipped; Decision 2 resolved |
| [0030](0030-crdt-merge-tier.md) | CRDT merge tier — add-wins set + server-serialized counter | Ratified, in flight — Decision 1 (counter delta-op) shipped |
| [0031](0031-sync-rules-modes-and-checksum-resync.md) | Three-mode sync rules and checksum-gated resync | Accepted — implemented |
| [0032](0032-unified-api-contract.md) | Unified API contract (typed reads, structured predicates, outbox batching, dead-letter observability) | Accepted — Wave 1 (Flutter) implemented |
| [0033](0033-browser-durable-storage-execution.md) | Browser-durable storage execution (ADR-0017 follow-up) | Implemented |
| [0034](0034-attachments-two-plane-blob-sync.md) | Attachments — two-plane blob sync (T6) | Accepted (Wave 3 shipped) |
| [0035](0035-wasm-typed-verb-surface.md) | WASM typed-verb surface (Wave 4a) | Implemented — browser Playwright verification pending |
| [0036](0036-flutter-web-engine-selection.md) | Flutter-web engine selection (shared nostos-ffi-wasm over frb.web) | Implemented (Wave 4b + 4c) |
| [0037](0037-sync-aware-push-notifications.md) | Sync-aware push notifications — predicate-routed doorbell + visible templates | Accepted |
| [0038](0038-standalone-push-daemon.md) | Standalone push daemon (nostos-pushd) + optional RemoteNotifier delegation | Accepted |
| [0039](0039-sync-streams.md) | Sync streams — server-defined, client-parameterized subscriptions | Accepted (implemented) |
| [0040](0040-bounded-sink-loss-windows-and-slow-client-policy.md) | Bounded-sink loss windows and slow-client policy | Accepted (implemented) |
| [0041](0041-transport-abstraction-ws-iroh.md) | Transport abstraction — `ws` \| `iroh` as a first-class server/client option | Accepted — gated items in its Acceptance section |
| [0042](0042-mirror-ingest-sidecar.md) | Mirror ingest for the desktop-sidecar topology | Accepted |
| [0043](0043-slot-max-lag-default-and-abandoned-slots.md) | `NOSTOS_SLOT_MAX_LAG` defaults to 1 GiB; abandoned slots are a separate failure mode | Accepted (shipped) |
| [0044](0044-cargo-build-dir-and-cache-hygiene.md) | Cargo intermediates live on the external SSD via a global `build-dir`; caches are swept weekly | Accepted — operator-machine policy |
| [0045](0045-per-key-conflating-session-sinks.md) | Per-key conflating session sinks | Accepted |
| [0046](0046-rename-fallback-env-config-binaries.md) | Pre-rename env, config and binary names keep working until 1.0 | Accepted |
| [0047](0047-push-presentation-options.md) | Push presentation options — one vocabulary, every rail | Accepted |
| [0048](0048-held-identity-migration.md) | The held identity migrates — state in place, wire by hard cut | Accepted |
| [0049](0049-per-principal-local-retention.md) | Per-principal local retention — an opt-in that keeps rows across sign-out | Accepted |
