---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "High. Cloud migrations, Function protocol, durable cursors, and Flutter/native entrypoints would all need migration."
  surprising_without_context: true
  surprise_reason: "ADR-0023 deferred Appwrite and described a server-side Realtime adapter; direct mode instead uses an Appwrite Function and durable TablesDB journal."
  result_of_real_tradeoff: true
  rejected_alternatives: "Realtime alone has no durable replay cursor. Direct SDK table writes bypass the journal. An always-on Nostos server would defeat the requested default direct mode."
  all_three_true: true
status: accepted
---

# ADR-0050: Appwrite direct sync through a Rust Function and TablesDB journal

- **Status:** Accepted (2026-09-26)
- **Amends:** ADR-0023 D4's deferred Appwrite sketch. It does not change the Postgres/Supabase server adapter or Supabase direct protocol.
- **References:** [research](../research/atlet-appwrite-tablesdb-sync-2026-09-26.md), [Atlet plan](../plans/atlet-appwrite-flutter-design-2026-09-26.md), `apps/atlet/appwrite/`, `crates/nostos-core/src/appwrite_pull.rs`

## Context

Appwrite Cloud exposes TablesDB transactions, Auth, Functions, Realtime, and
Messaging, but its Realtime delivery is not a replayable change feed. A device
that goes offline needs to fetch every committed change and every delete after
its last durable checkpoint. Appwrite does not expose the Postgres logical
replication stream expected by the original server adapter sketch in ADR-0023.

The Atlet reference app must show one admin and multiple customers using the
same hosted project. Direct mode is the default across providers. Appwrite's
row permissions alone cannot guarantee that a data write and its feed entry
are committed together when an arbitrary client writes directly.

## Decision

1. An Appwrite-authenticated device calls the hosted `atlet_sync` Rust Function
   with a short-lived user JWT. The Function verifies the user and admin team
   membership for each request. Its dynamic server API key never reaches an
   SDK. Synced TablesDB rows have no direct client write permission.
2. Every accepted mutation, full row image or tombstone, monotonic sequence,
   and idempotency key commit in one TablesDB transaction. A single `sync_clock`
   row serializes writers. The Function retries transaction conflicts. The
   journal is retained until a snapshot and retention protocol is proven.
3. Pull responses include `head` and `scanned_through` decimal strings, plus
   the verified principal ID and current admin/customer access scope. A scope
   change clears cached rows and restarts the pull from zero. The
   Function filters private rows; an explicit inactive-profile rejection wipes
   the local cache and outbox before returning an error. The Flutter bridge
   surfaces a distinct access-revoked state so even a first-sync rejection
   presents a sign-out path. The client advances
   past scanned hidden entries only after visible rows apply and its cursor
   saves. Realtime and
   push may wake the client, but neither is the source of truth.
4. Nostos's Rust apply engine and SQLite outbox remain the client data path.
   Appwrite direct has a distinct transport and cursor because Supabase's
   transaction horizon cannot represent a filtered Appwrite page with no
   visible rows. Provider data uses separate local storage. The stored
   principal includes endpoint, project, Function, user, and role; changing
   any cloud identity wipes old rows and outbox. Sign-out clears local state.
   Pre-release test installs that stored the earlier user-only principal
   reset on first open; drain their pending test writes before upgrading.
5. Numbered, immutable JSON migrations in `apps/atlet/appwrite/migrations/`
   define the cloud schema. A Rust runner checks their hashes in
   `schema_migrations` before applying missing steps. Cloud smoke runners are
   Rust binaries in `apps/atlet/harness`. A disabled user profile cannot call
   the Function, and the admin cannot disable their own sync access.

## Consequences and limits

The first implementation is the full visual Atlet Flutter app on native
macOS, backed by the real ADS Appwrite project. Its hosted order test covers
offline checkout, fulfilment, durable events, and customer isolation. The
Function is a separate Cargo workspace because Appwrite Cloud currently
builds with Rust 1.83; `scripts/check.sh appwrite-function` and the matching CI
job run its format, Clippy, tests, and dependency checks independently.

An admin role change must invalidate cached private rows before subsequent
pulls can continue. A device that stays offline cannot learn that a remote
role changed; local device access and sign-out policy remain part of the app's
security boundary. Flutter web, Appwrite server mode, and physical push are
separate acceptance gates before the wider SDK reference is complete.
