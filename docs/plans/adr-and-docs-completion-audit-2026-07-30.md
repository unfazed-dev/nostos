# ADR + docs completion audit — 2026-07-30

**Question asked:** are all 28 ADRs and the `docs/` tree actually complete, and is nostos missing
anything it should have built?

**Method:** extracted every ADR's declared status, then **verified the suspicious ones against
code** rather than trusting the status line. That ordering is the whole point — earlier today the
Flutter redesign plan claimed "no implementation without operator go" while six of its seven
decisions were shipped and exported. Status lines lie in *both* directions.

## Headline

The engine is **more complete than its own ADRs claim.** Six status lines understate reality,
including the one covering nostos's headline moat. There are four genuine unbuilt gaps, and only
two of them plausibly block a launch.

## Status lines that are WRONG (docs claim less than reality)

| ADR / doc | Declares | Reality | Evidence |
|---|---|---|---|
| **0013** Direct write-back | "**Deferred** (Phase 4 — design sketch)" | **SHIPPED** | `PgWriteBack` in `nostos-infra`; two dedicated real-PG tests (`e2e_pg_writeback.rs`, `e2e_pg_writeback_timestamp.rs`); `NOSTOS_WRITE_TABLES` gate at `main.rs:112`; it is the ECHO half of **all 10** `sdk-e2e` slices |
| **0015** FFI bridge strategy | "WASM shipped; **Flutter / RN / Node-native remain**" | all three **SHIPPED** | each has a passing `sdk-e2e` slice |
| **0016** Client SDK | "**FFI bridges remain** (ADR-0015)" | same — shipped | as above |
| **0023** `.nostos/` project dir | "**Proposed**" | **SHIPPED** | `DOT_NOSTOS_DIR` + `config.json` + `schema.json` in `nostos-cli/src/config.rs:178`; all 7 CLI commands exist |
| **0024** Reactive facade | "**proposed**" | **SHIPPED** | `Collection<T>`/`SyncStatus` exported; cited by `nostos_facade_test.dart` and 4 demo files |
| **ROADMAP:54** Flutter SDK | "(ADR-0015 — **deferred**.)" | **SHIPPED** with a first-class `Stream` | `NostosDatabase.watch` |

**0013 is the one that matters.** The landing page sells "Direct Write-Back — no more
`uploadData()` endpoints" as the differentiator against PowerSync, and `STRATEGY.md` builds the
competitive argument on it. A reader who checks the ADR is told the feature is a deferred design
sketch. That is the single most misleading line in the repo.

## Status lines that are ACCURATE

0001–0012, 0014, 0017–0022, 0025–0028. Spot-checked rather than assumed:

- **0014** "LWW shipped, CRDT/custom deferred" — correct. Grep for CRDT across `crates/` returns
  exactly one hit, a doc mention in `nostos-core/src/lib.rs`. Genuinely not implemented.
- **0017** "Deferred past v0.1" — correct, verified in detail earlier today.
- **0012** "Moat complete, slices 1 & 2; param-set-digest indexing deferred **by data**" — correct
  and unusually honest: the index was built, measured a 4–8× regression, and reverted.

## Genuinely NOT implemented

Ranked by whether they block a launch.

### 1. Token refresh — ~~blocks a Supabase launch~~ **FIXED 2026-07-30**
Was: `ClientConfig.token` immutable, reconnect loop re-sends it, server enforces `exp` ⇒ a Supabase
app stopped syncing about an hour after login and never recovered.

Now: `SyncClient.set_token` (token behind a `RwLock`, read by `connect_url()`) → `NostosHandle::
set_token` (updates the seed *and* the live client) → `Nostos.setToken` → `NostosDatabase.supabase`
auto-wires `onAuthStateChange`, cancelled in `close()`.

**The ratified fix was wrong and got replaced.** "Pure Dart, no Rust change" required rebuilding the
handle, because the token is constructor-baked and no swap primitive existed (the docstring claiming
`NostosSupabase` had one was false). Rebuilding ends every `watch` stream —
`_replayLatest` wires `onDone: controller.close` — so the app would appear to lose its data an hour
after login instead of silently not syncing. Strictly worse. The FFI route touches no stream.
Guarded by `set_token_changes_the_next_connect_url` plus three Dart tests, one of which asserts an
active `watch` stream survives a refresh.

### 2. Web durability (ADR-0017) — blocks a *web* launch, nothing else — **NOT FIXED, deliberately**
The browser keeps rows in an in-memory `BTreeMap`. A reload loses everything except the
`localStorage` checkpoint. Destination chosen (SQLite-WASM + `opfs-sahpool`); the blocker is the
Worker re-architecture, not SQL. Native platforms are unaffected.

**Scope corrected 2026-07-30:** the browser also has **no outbox on the write path**, so it is a
*live-only* client rather than a durable-minus-reload one. Both limits are now stated in
`sdk/nostos_web/README.md`, whose Ceiling section had claimed "the remaining gap is Node-only" —
wrong, and wrong in the flattering direction.

**Left unbuilt on purpose, and it is the one item in this audit I am not closing.** ADR-0017 is a
*ratified* deferral, and the work it names is not a bug fix — it is: spawn a dedicated Worker,
define a `postMessage` command/response protocol, marshal `RowOp`/`PendingWrite` across it, and move
the WebSocket transport into the Worker too (it cannot call sync storage from the main thread).
Multi-day, and by the ADR's own admission with "no Node-verifiable test path". Building that inside
a fix-up pass would be the opposite of the discipline the rest of this audit applies.

**The cheaper IndexedDB option I raised here was examined and REJECTED** — see the 2026-07-30
addendum to ADR-0017. Recording it against myself, since two of my claims above were wrong:

- **The gap is bigger than "rows".** `NostosSocket::write` (`nostos-ffi-wasm/src/lib.rs:496`) sends
  the frame straight to the socket and **never touches `Outbox`** — with the socket closed it
  **throws** (wasm-bindgen turns the `Err` into a thrown exception). The browser is therefore a
  **live-only** client: no offline write capture, no optimistic local row. Not silent, but row
  durability alone would not make it offline-capable. ADR-0017 predates this surface (ADR:
  2026-07-04; `write`: 2026-07-12), which is why its Consequences reason only about rows.
  Scoped to `NostosSocket` — `NostosClient.write` on the Node facade feeds the apply engine and opens
  no socket at all.
- **"No transaction spanning a batch" was wrong.** IndexedDB *has* transactions and they span
  object stores, so rows + checkpoint could commit atomically. The real blocker is that its API is
  **async** while `Storage`/`Outbox` are **sync** — no sync trait method can await an IDB request,
  so IndexedDB can only be a write-behind mirror, never a `Storage` impl.

Rejected because the mirror fixes the visible half and leaves the half that matters: rows would
survive a reload while an offline write still throws, which *looks* offline-capable and isn't. Plus
it would have to demote the `localStorage` checkpoint (otherwise the checkpoint runs ahead of the
mirrored rows and resume permanently skips the rows in between), and SQLite-WASM deletes it later.

Also re-priced while writing the addendum — **and I first got this wrong too.** I compared 13 total
methods against ADR-0017's 5 and called it 2.6×; the ADR's 5 were the *required* ones, so
required-vs-required is **7 vs 5 = 1.4×** (ADR-0025 added `pks_for_table`/`delete_pks`). The other 6
have defaults, but the defaults *degrade*: no dead-letter quarantine (ADR-0027), no instant-local
row, no oplog epoch check. So 1.4× for a correct Worker backend, ~2.6× for one at parity with
native. Either way the deferred slice got more expensive while sitting still.

The reject-the-mirror call above is mine as tech lead, not an operator ratification — the operator
asked for the amendment and the audit had left the option open. Point 3 of the addendum is the
argument to attack if it should be overturned.

### 3. CRDT / custom merge tier (ADR-0004, ADR-0014) — does not block
Per-field LWW ships and is the documented default. CRDT was always "Phase 4, opt-in".

### 4. Reactive facade outside Flutter — does not block
Eight SDKs poll. Not architectural; ADR-0024 simply targeted the launch platform.

### Also open, already honestly documented
- **10k clients: the <1%-drop goal was NOT met** — ~61.4% drops at 10k (`ROADMAP:14`). 1k is
  833k ops/sec at 0.00%, which is the number the moat rests on. Named follow-up: table-sharded
  router. Recorded honestly; do not quote a 10k figure.
- **iOS TurboModule for RN** — Android only; `nostos_swift` is sim-proven so the pieces exist.
- **Registry publishing** — `publish = false` on 5 crates; `@nostos-sync/capacitor` has a `file:` dep.
- **Identity** — no domain/entity/mailbox (`git grep NOSTOS-IDENTITY-PENDING`).
- **Non-`public` Postgres schema** — `view_name` collapses the dot to `myschema_tasks` while
  `Collection.watch` emits `myschema.tasks`. Untested, plausibly broken (ADR-0028).

## Doc-tree defects

1. **Two docs whose titles both read as "Security Model."** `docs/SECURITY.md` (111 lines) is the
   *operational* one — collapsed-write model, least-privilege `BYPASSRLS` role, Supabase setup,
   RLS trade-off. `docs/SECURITY-MODEL.md` (63 lines) is the *conceptual* one — why RLS cannot
   reach sync traffic. Complementary, **not** duplicates, but with near-identical filenames and a
   *third* `SECURITY.md` at the repo root (the vulnerability policy). Consequence, already
   observed: `docs/api/README.md` links only `SECURITY-MODEL.md`, so a deployer following it never
   reaches the least-privilege role setup they actually need.
2. **A stale git worktree ships pre-fix content.** `.claude/worktrees/agent-a829d134a1183217c/`
   is a leftover, unregistered in `git worktree list`, containing the **fabricated `nostos-sync`
   org**, a live `mailto:founders@nostos.run`, the old `SECURITY.md` telling researchers to email
   that dead mailbox, and the stale "7 slices" text. It is not built or shipped, but it is inside
   the tree and greppable — it produced false positives in this very audit. Any agent grepping for
   current state can read it as authoritative.
3. `docs/WEEK-01-PLAN.md` is historical; fine, but nothing labels it as such.

## Conclusion

Nothing architecturally required is missing. The gap between nostos and "launchable" is **one P1
(token refresh)** plus **publishing and identity**, both of which are operator decisions. Web is
launchable as a demo but not as a durable app.

The dominant risk in this repo is not unbuilt features — it is **documentation that misreports
shipped state in both directions.** That is now the third instance today (the Flutter plan,
ADR-0021's "typed tables", and these six status lines).
