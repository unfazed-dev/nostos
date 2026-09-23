# Nostos Unified API Contract (v1)

**Status:** ratified by operator 2026-08-07 (grilling session) · advisor-reviewed (GLM-5.2, HIGH confidence)
**Scope:** one verb contract for all 9 SDKs. No SQL in application code. SQL `execute()` survives only as an explicitly-documented escape hatch, never the front door.
**Supersedes:** the *documentation posture* of ADR-0028 (SQL-primary Flutter surface). Does NOT reverse ADR-0028's engine decisions (views over `cairn_data`, no materialized typed tables) — the typed layer sits on top, exactly as ADR-0024 built it.

## Why (evidence, 2026-08-07)

- The pilot app (`apps/atlet/flutter`) hand-writes SQL for every read
  (`db.watch('SELECT * FROM sessions …')`, nostos_adapter.dart:107–132) even though
  `Collection<T>` exists — because docs/api/flutter.md presents SQL first.
- The docs are NOT stale: all 9 authored 2026-07-30 (`3a115fc`), flutter.md signature-checked
  (`check-doc-signatures.py`, exit 0 on 2026-08-07). The gap is *contract*, not rot:
  8 SDKs have no typed layer at all, and even `Collection<T>` leaks SQL fragments in
  `where:`/`orderBy:` with parameter binding an unshipped P1 (injection foot-gun).
- Every Nostos read in atlet is "table, maybe filter, maybe order" — zero joins/subqueries.
  Local-first datasets are pre-scoped by sync rules (ADR-0031), so app queries stay simple.

## Query shape: structured predicates (no expression language, no builder)

`where` is **data**, not strings. Mirrors `Predicate` in nostos-domain; serializes over
UniFFI so all 9 SDKs share one implementation and one test suite. Kills the injection P1.

```dart
// Dart flavor — each SDK renders this idiomatically, semantics identical
todos.watch(
  where: Where.and([Where.eq('completed', 0), Where.gt('due_at', now)]),
  orderBy: [Order.desc('created_at')],
  limit: 50, offset: 0,
)
```

Operators v1: `eq, neq, lt, lte, gt, gte, inList, isNull, notNull` + `and, or, not`.
A fluent per-language builder is explicitly **rejected** for v1 (9 codegen surfaces for
zero added expressiveness). Revisit only when a real app's predicate outgrows this set.

## The verb contract

Names below are canonical; each SDK uses its language casing (`waitForFirstSync` /
`wait_for_first_sync`). **Every verb ships on every SDK** unless marked.

### T1 — Lifecycle & auth
| Verb | Semantics | Today |
|---|---|---|
| `open(schema, path)` | create/open local db | ✅ all |
| `connect(endpoint, token)` | start sync | ✅ all |
| `disconnect()` | stop sync, keep data | ✅ all |
| `close()` | release resources | ✅ all |
| `setToken(token?)` | live credential swap — never reconnect to refresh | ✅ Flutter; port |
| `signOut()` | disconnect + **wipe** local data (ADR-0029: full-wipe IS the isolation) | ✅ all 9 |
| `status` | reactive `SyncStatus` (conn, hasSynced, lastSyncedAt, pendingWrites, deadLetteredWrites, lastWriteError) | ✅ Flutter rich; others partial |
| `waitForFirstSync()` | **NEW** — awaitable initial-sync barrier (comparable-SDK parity; today devs poll `hasSynced`). Must also resolve correctly on reconnect (advisor followup) | ❌ |

### T2 — Reads (all reactive verbs have one-shot twins)
| Verb | Semantics | Today |
|---|---|---|
| `collection<T>(table, fromRow, toRow?, pk)` | typed handle | ✅ Flutter only; **port to 8** |
| `get(pk)` | **NEW** one-shot single row (`fetchById` parity) | ❌ |
| `getAll(where?, orderBy?, limit?, offset?)` | one-shot list | ✅ Flutter (SQL-frag args → predicates) |
| `watch(where?, orderBy?, limit?, offset?)` | reactive list | ✅ Flutter (same migration) |
| `watchOne(pk)` | **NEW** reactive single row (WatermelonDB `findAndObserve` parity; detail screens shouldn't re-render on list churn) | ❌ |
| `count(where?)` / `exists(where?)` | reactive count / **NEW** cheap boolean | ✅ / ❌ |

### T3 — Writes (all through the durable collapsed outbox, ADR-0013; return outbox id, NOT server ack; applied state round-trips via `watch`)
| Verb | Semantics | Today |
|---|---|---|
| `upsert(T)` / `upsertRow(map)` | full-row collapsed write | ✅ Flutter |
| `patch(pk, columns)` | per-field LWW partial update (ADR-0014) — the canonical edit path | ✅ Flutter |
| `delete(pk)` | delete by pk | ✅ Flutter |
| `writeBatch([...])` | **NEW** — *all-or-nothing delivery*: the group enters/leaves the outbox atomically and uploads together. **Explicitly NOT a server transaction** — server applies rows individually with per-field LWW; no cross-row rollback. Docs must say this verbatim (advisor: HIGH risk if it merely *looks* transactional). Verify outbox same-field collapse within one batch before shipping | ❌ |

### T4 — CRDT typed surface (engine shipped in WS3 @317b4d1; unexposed)
| Verb | Semantics |
|---|---|
| `counter(pk, column).increment(n)` / `.decrement(n)` | commutative counter — the no-conflict alternative to `patch` for tallies |
| `orSet(pk, column).add(v)` / `.remove(v)` | OR-set element ops |

Docs teach the choice: `patch` = last-writer-wins field; `counter`/`orSet` = merge.

### T5 — Outbox observability
v1: counts in `SyncStatus` (exists) **+ `deadLetters()` read-only list** (id, table, error, timestamp) so failures are diagnosable.
v1.1 (deferred, advisor-ratified): `retryDeadLetter(id)` / `discardDeadLetter(id)`. Amends ADR-0027's counts-only stance for the list, not yet for mutation.

### Escape hatch
`execute(sql)` / `watchSql(sql)` remain, documented **last**, under "Escape hatch", with a
lint-friendly name that makes SQL usage greppable in app code.

## Excluded from v1 (deliberate, with re-entry triggers)
| Excluded | Why | Add when |
|---|---|---|
| Attachments/blobs | RxDB has it; no Nostos app needs it yet | first app with media |
| Relations/joins | compose two `watch` streams; document the pattern | measured pain in a real app |
| Client migrations | view-based reads (ADR-0028) make schema evolution server-side | native columns ever materialize |
| Fluent query builder | zero expressiveness over predicates, 9× codegen cost | predicate set outgrows v1 operators |
| Pause/resume sync granularity | `disconnect()` covers it | background-fetch platform work |

## Coverage validation (survey 2026-08-07)
Checked against: [WatermelonDB CRUD](https://watermelondb.dev/docs/CRUD) (write-txn, observe, findAndObserve), [RxDB](https://rxdb.info/alternatives.html) (reactive queries, CRDT, attachments), Triplit/InstantDB/[Zero](https://marmelab.com/blog/2025/02/28/zero-sync-engine.html) (typed queries, transact, fetchById, pagination). Every competitor verb is either in the contract, consciously excluded above, or an artifact of their architecture (e.g. Zero's server-side ZQL).

## Sequencing (ratified)
1. **This contract** (done — this document).
2. **Flutter port**: predicates replace SQL-frag `where:`/`orderBy:`; add `get`, `watchOne`, `exists`, `limit/offset`, `waitForFirstSync`, `writeBatch`, CRDT surface, `deadLetters()`. Rewrite docs/api/flutter.md typed-first; keep `check-doc-signatures.py` green.
3. **Fix atlet**: nostos_adapter.dart → `Collection<T>` only; target = `grep -cE 'SELECT|INSERT|UPDATE|DELETE' lib/` returns 0.
4. **Port to remaining 8 SDKs** (UniFFI carries predicates + typed layer; wave order TBD), then regenerate all 9 docs/api/*.md from the shared contract with per-SDK signature checks.

## Advisor followups (open)
- Verify outbox collapses same-field writes *within* one batch correctly.
- Spec client retry/backoff policy in the contract docs.
- `waitForFirstSync` semantics across reconnects.
- ADR to write at implementation time: next free number (0032), citing this plan.

---

## Amendment v1.2 — Tier-1 SDK extension (ratified 2026-08-08, grilling session)

Tiering by SDK replaces the v1 blanket exclusions. The **core contract (T1–T5)
above applies to all 9 SDKs** unchanged. A **tier-1 extension** applies to two
SDKs only: **Flutter** and **Web (JS, `sdk/nostos_web`)**. Flutter-compiled-to-web
is in scope but **trails** (wave 4 below) — `nostos_flutter` has no web target
today; it gains one by **extending the shared `nostos-ffi-wasm` backend**. *(Premise
corrected 2026-08-08: that backend was apply-engine-only until Wave 4a ported the
typed verbs onto it — the original "same WASM path the JS SDK uses" wording assumed
a full client that did not exist. `frb_generated.web.dart` exists but is rejected:
it compiles the rusqlite-based Flutter Rust crate to wasm and strands Flutter-web
without Wave-2 opfs-sahpool durability. See ADR-0035.)*

### Tier-1 verbs (previously "Excluded from v1")

| Verb | Semantics |
|---|---|
| `pauseSync()` / `resumeSync()` | Intent-carrying sugar over disconnect/connect. Keeps token, schema, and watch subscriptions alive; streams resume without re-wiring. No new protocol state. |
| T6 Attachments | Two-plane design, below. |

### T6 — Attachments (two-plane, BYO blob storage)

- **Metadata plane (Nostos syncs it):** an attachments table (`id, filename,
  size, media_type, state, timestamp`) synced through the normal
  replication/outbox path. State machine:
  `QUEUED_UPLOAD | QUEUED_DOWNLOAD | QUEUED_DELETE | SYNCED | ARCHIVED`.
- **Blob plane (developer supplies it):** `AttachmentStorageAdapter`
  interface — `upload(path, bytes, mediaType)`, `download(path)`,
  `delete(path)`. Nostos decides *when* (connectivity, retry, ordering vs the
  referencing row); the developer's bucket decides *where*. First-class
  `SupabaseStorageAdapter` ships; everything else is interface-only.
- **Blobs never transit the Nostos server.** This is a moat constraint, not a
  convenience: proxying blobs would pollute fan-out throughput and make the
  server stateful. Same posture as comparable sync SDKs' remote-storage-adapter interfaces.
- Queue implementation lives **once, in `nostos-core`** (WASM-clean), surfaced
  to Flutter (filesystem blob store) and Web (OPFS blob store).

### Web durability dependency (the big rock)

Web today is **live-only** (ADR-0017 as amended 2026-07-30: only `resume_lsn`
in localStorage; no outbox, no local rows, IndexedDB mirror rejected). Tier-1
on web therefore **executes ADR-0017's follow-up**: a browser-durable `Storage` impl
(official SQLite-WASM with the `opfs-sahpool` VFS — ADR-0017's decision, which
explicitly REJECTED wa-sqlite/`OPFSCoopSyncVFS` for its COOP/COEP deployment tax;
header-free deployment, no cross-origin isolation required; Safari Private
Browsing degrades to the in-memory backend). T3 writes, T5 outbox surfaces, and T6 on web are
all gated on that work. Done in nostos-core behind the `Storage` trait, it is
paid **once** and serves both JS-web and Flutter-web.

### Waves (ratified order; Flutter-web trails)

| Wave | Work | Serves |
|---|---|---|
| 1 | Flutter core-contract port + `pauseSync`/`resumeSync` + atlet de-SQL | Flutter native |
| 2 | Browser-durable Storage in nostos-core (ADR-0017 revision) | JS web + (later) Flutter web |
| 3 | T6 attachments in nostos-core + BYO adapter + `SupabaseStorageAdapter` | Flutter native first; web when 2 lands |
| 4 | Flutter-web binding (nostos-core WASM engine path under the same Dart API) | Flutter web |

Implementation plan for executor agents:
`docs/plans/nostos-unified-api-implementation.md`.
