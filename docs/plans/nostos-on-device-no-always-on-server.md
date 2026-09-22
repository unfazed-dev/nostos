# Nostos with no always-on server — the on-device shapes and what each costs

**Date:** 2026-09-22. **Status:** topology **decided** — one shared database per
app, shipped to an agency's clients, and **no Nostos server anywhere**. That
combination is possible; it is Shape B2 below. The earlier drafts of this file
answered a narrower question and got the conclusion wrong — see "The correction"
.
**Prompted by:** the operator's constraint, stated plainly — *"I just do not want
to involve another device that stays on all the time for Nostos to work."*

The constraint is legitimate and the current answer ("run nostos-server on a VM
or a spare box") is a real adoption tax. This document maps the shapes that
satisfy the constraint, and is honest about which one dies where.

**The correction.** Drafts 1–3 of this document treated "no Nostos server" as
"keep logical replication, move it to the phone", concluded that a shared
database makes that impossible, and therefore that the server is mandatory.
The second step is right and the conclusion does not follow. **Logical
replication is one implementation of Nostos's change-source port, not the
definition of Nostos.** ADR-0023 D4 already says so in as many words: the
Appwrite adapter "does not expose Postgres logical replication, so its adapter
implements the change-source port over Appwrite's realtime/events API and the
write-back port over its REST API. The port seam is exactly where that
difference is absorbed."

Move that adapter from the server to the device and the requirement disappears —
without moving a replication slot, a `REPLICATION` credential, or a Postgres
anywhere near a phone.

## The reframe that matters

**`nostos-server` is not a process you must keep alive. It is a library.**

`nostos-server` is a composition root over `nostos-infra`; the session logic lives
in `nostos-infra`'s transport module, not in the axum handler. `README.md`'s
zero-setup demo already runs an **in-process** sync server plus a durable SQLite
client in one process, restarts the server mid-run, and prints
`resumed from durable checkpoint`.

So "the phone runs the server" does not require a background daemon. It requires
the router to be *linked into the app*. The lifecycle then needs no continuous
uptime at all:

| phone state | what happens | mechanism |
|---|---|---|
| app open | replicator streams WAL → local SQLite, live | in-process router |
| backgrounded / killed | connection drops — this is what offline-first means | — |
| reopened, or after reboot | resumes from the durable LSN checkpoint | ADR-0025 |
| closed, data changed | OS push doorbell wakes the app | ADR-0037 |

`docs/STRATEGY.md:214` already states the discipline: push is a **wake-up
trigger, not a data channel**. That is precisely the primitive this shape needs.

## Why the literal "server running in the background" is closed

Not a permissions problem — a platform-policy one, and it is getting stricter.

**Android 15+:** a `dataSync` foreground service may run **6 hours per 24-hour
period**; then `Service.onTimeout()` fires and the service must `stopSelf()` or
the system kills it with `RemoteServiceException`. The budget only resets when
**the user brings the app to the foreground**. Apps targeting Android 15+ may
**not** launch a `dataSync` foreground service from a `BOOT_COMPLETED` receiver,
which forecloses "comes back automatically after a restart". `dataSync` is
slated for deprecation; the named successors are User-Initiated Data Transfer,
WorkManager expedited work, and `shortService`.

**iOS:** no boot launch for apps at all; background sockets stop roughly 30 s
after screen lock by design; Apple DTS answers "No" to keeping a persistent
background connection alive, and notes `BGProcessingTaskRequest` is delivered
"typically overnight when the user is asleep".

**Conclusion:** design for resume, not for uptime. The table above is
achievable; a background daemon is not.

## The three shapes

### Shape A — embedded replicator (the router linked into the app)

The app's Rust lib links `nostos-infra` with the `pg` feature and holds the
logical-replication connection itself. **Every install runs its own router.**
Nobody's phone serves anybody else's, and no second device exists anywhere in
the picture.

**Works:** no second device, no hosting bill, full Nostos semantics (predicates,
LSN checkpoints, op-log resume, write-back).

**The number of routers is not the constraint. The number of slots is.** A
replication slot is a property of the *database*, not of the router — so the
count that decides whether this ships is **slots per database**, and that
depends entirely on one question:

#### A1 — one shared Postgres behind every install

The normal shape for a shipped app: you own one database, all users read from
it. Each embedded router opens its own logical-replication connection to that
one database, so N installs means N slots — and `max_replication_slots`
defaults to **10**, with the standing advice to keep it tight. Each connection
also burns a `max_wal_senders` slot and a walsender process.

Three failure modes, all of them properties of the shared database:

1. **Slot count scales with installs**, not with users-per-device. 1,000 users
   with one phone each is still 1,000 slots on your one database.
2. **One laggard pins WAL for everyone.** Each slot retains WAL from its own
   confirmed position, so a single phone offline for days sets the retention
   floor for the whole database. Disk fills, writes halt — the ADR-0043 class,
   multiplied by the install count.
3. **Uninstall leaves an abandoned slot.** Nothing on the device can clean up
   once the app is gone. `max_slot_wal_keep_size` bounds the damage by
   invalidating the slot, at the cost of a full resync for any device that was
   merely asleep.

It also puts a `REPLICATION`-privileged credential in the app bundle, where any
user can extract it and stream the whole database.

**A1 is what needs `nostos-server`** — one process holding the single slot and
fanning it out to many sessions. That is the multiplexer, and nothing on a
phone can replace it.

#### A2 — one Postgres per user

Each install talks to a database that only its own owner holds: their own
Supabase project, their own Neon/Fly instance, their own box. Every objection
above evaporates, because each one was about *sharing*:

- **One slot per database.** The ceiling never binds — one database, one
  consumer, and that consumer is the owner's own phone.
- **A laggard pins only its owner's WAL.** The blast radius of being offline
  for a month is your own disk.
- **An abandoned slot is the owner's own.** No cross-user damage, and `nostos
  doctor` can reap it on next launch.
- **No credential in the bundle.** The user supplies their own connection
  string at setup, so nothing privileged ships in the binary — which is also
  the only way a `REPLICATION` role is ever acceptable on a device.

**A2 satisfies the operator's constraint with no caveat at all:** no always-on
server, no second device, per-install Nostos, full semantics, at any number of
users. The cost did not vanish, it moved — onboarding now includes "bring a
database", and provisioning one per signup is a product you'd have to build
(this is the Turso / Neon / per-tenant-project pattern, and it is a real one).

**Therefore:** the fork is not phone-vs-server. It is **shared database → you
need nostos-server; database-per-user → you don't.**

#### What A1 settles (operator decision, 2026-09-22)

> *"One shared database per app, like most apps."*

So A1 it is, and that closes the embedded-replicator question for the flagship
story. Not on compute grounds — a phone has plenty — on three grounds that are
properties of sharing one database:

1. **Slots.** N installs would need N slots on the one database, cap ~10.
2. **Security, and this one is fatal on its own.** A shared database means the
   bundled credential is a key to *other people's rows*. A `REPLICATION` role
   streams the entire WAL — every table, every tenant — and RLS does not apply
   to logical replication. Any user can extract it from the APK. There is no
   version of this that ships.
3. **WAL retention.** One phone offline for a fortnight pins the retention
   floor for every user's data.

**So an embedded *replicator* is closed for a shared database.** Not
"Nostos on the device" — the *replicator*. A2 stays as a documented power-user
path (self-hosters, one-owner apps), not as a default.

What A1 actually proves is narrower than drafts 1–3 claimed: **if logical
replication is your change source, you need something in the middle to
multiplex the one slot.** It does not prove that something has to be
`nostos-server`, or yours, or a process you pay for. Supabase already runs one
(Shape B2). The cost of running your own is priced below for comparison, then
Shape B answers the actual question.

## What an always-on server costs, if you choose one (A1 baseline)

Checked 2026-09-22 against the official pricing pages. `fly.toml` already
targets Fly, and `auto_stop_machines = false` is mandatory there (a stopped
nostos-server stops consuming its slot, `max_slot_wal_keep_size` is exceeded,
`wal_status` flips to `lost`, and changes are silently skipped — that is a
data-loss server, not a paused one).

| item | cost |
|---|---|
| Fly `shared-cpu-1x`, 256 MB, never stopped | **$1.94–$2.24 / month** by region group |
| 512 MB, if 256 is not enough | $3.19–$3.69 / month |
| Egress, NA/EU | $0.02 / GB (all inbound free; same-region free) |
| Supabase Free Postgres | $0 — 500 MB database, 500 MB RAM, shared CPU |

**An always-on server is about two dollars a month per deployment.** Worth
knowing, but note what it means for an agency: it is $2/month *per client*, plus
a deploy, a monitor, an upgrade path and a pager for each one. The objection was
never the two dollars — it is owning centralised infrastructure on behalf of
other people's products. Shape B removes the box, not just the bill.

Three caveats that are real:

- **We have never measured nostos-server's memory footprint.** 256 MB is a
  guess. Per-session RSS is unmeasured, and the 10k-session ladder in
  `benches/results/linux-unfazed-rog-2026-09-22.md` only reports throughput.
  Measuring RSS per session is the cheapest missing number in the project and
  it is the one that decides the $1.94 row from the $3.19 row.
- **Egress scales with fan-out, and can exceed the machine.** At ~300 bytes per
  JSON frame: 1,000 users each seeing 500 relevant row-changes a day is ~4.5
  GB/month, about **$0.09**. The same 1,000 users all subscribed to the same
  100k changes a day is ~900 GB/month, about **$18** — nine times the machine.
  This makes predicates (ADR-0003) a **cost control**, not only a correctness
  feature, and that argument belongs in the docs.
- **Supabase Free pauses a project after 1 week of inactivity**, and allows 2
  active projects. Fine for development, but a paused project plus a
  replication slot is exactly the WAL-invalidation path above. Any "free tier"
  onboarding story has to say this out loud.

### Shape B — the backend adapter, running on the device (**the answer**)

No replication anywhere on the client side. `nostos-client` keeps everything it
already has — `ApplyEngine`, `SqliteStorage`, the read VIEWs (ADR-0028), the
reactive facade (ADR-0024), the durable outbox with its DLQ — and only the
*source of frames* changes: instead of Nostos's `/sync` WebSocket, the device
subscribes to the backend's own realtime surface and writes back through its own
REST API.

This is ADR-0023 D4's adapter seam, evaluated on the device instead of on a
server. Every piece of compute is on the phone, which is exactly the ask. There
is no Nostos process in the middle, for anyone, ever.

Two variants, and the difference decides whether it ships to an agency's
clients:

#### B1 — Postgres Changes (the naive one)

Subscribe to `postgres_changes` per table. Simple, and it is what
`brick_offline_first_with_supabase` does.

**Ceiling, from Supabase's own docs:** Postgres Changes "authorizes every event
against each subscriber… so throughput scales with the number of subscribers,
not the write rate", and changes are "processed on a single thread to preserve
their order, which means larger compute add-ons don't meaningfully increase
Postgres Changes throughput". Their guidance: past **~3,000 concurrent
subscribers on the same changes**, stop using it.

Note what that ceiling *is*: per-subscriber fan-out work done in one place. It
is the same axis `benches/results/RESULTS.md` measures, and the same reason
nostos-server exists.

#### B2 — Broadcast from Database (**recommended, and it scales**)

A Postgres trigger calls `realtime.broadcast_changes()`, which inserts into
`realtime.messages`; Realtime reads *that* table's WAL through **one**
publication and fans each message out over WebSockets. Supabase's docs call
Broadcast "the recommended method for scalability and security" and state it
"sends each change once and fans it out to all subscribers, so it scales to far
higher connection counts than per-subscriber authorization allows".

Read that mechanism against the problem this document started with:

| the problem | B2's answer |
|---|---|
| one slot cannot serve many devices | Realtime holds **one** publication, on `realtime.messages` |
| something must multiplex it | Realtime does, and it is already running |
| a privileged credential would ship in the app | none — the app carries the anon key and a user JWT |
| scoping must be server-authoritative | RLS, in Postgres, via private-channel authorization |
| who keeps it alive | Supabase, on the client's existing bill |

**The multiplexer still exists. It is just not yours, and not a new box.** That
is the whole difference, and it is the difference the operator asked for.

**What it costs, honestly.** The protocol has six non-negotiable rules, all of
them published by prior implementations and two of them published as warnings.
They are worked out with sources in
[`direct-mode-sync-protocol.md`](direct-mode-sync-protocol.md); in summary:

1. The checkpoint is **`(updated_at, pk)`**, never a bare timestamp — a bare one
   loses rows at page boundaries (RxDB, Confluent JDBC both say so).
2. `updated_at` is stamped by a **database trigger**, never by the client.
3. **Soft delete is mandatory**, with a purge window longer than the longest
   tolerated absence.
4. The realtime stream is a **doorbell**; every reconnect resyncs from the
   checkpoint before trusting a streamed frame (RxDB's `RESYNC`, and Nostos's own
   ADR-0037 doctrine).
5. The watermark is captured **before** the catch-up query, accepting duplicate
   delivery — free for Nostos, whose `(table_name, pk)` row images are idempotent.
6. Private channels need RLS on `realtime.messages` **and** "Allow public
   access" disabled — a setting, not a policy.

Rule 4 is what retires the ~3-day `realtime.messages` retention: a device away
for a fortnight takes the same code path as one that dropped a socket for a
second.

**And one thing direct mode cannot have: cross-table transactional
consistency.** Per-table watermarks mean a device can hold an order line whose
header has not arrived. PowerSync built a service for exactly this and had it
Jepsen-verified. That is the strongest remaining argument for `nostos-server` —
and it is a correctness argument, which is a better one than throughput.

Plus: it is only available where the backend *has* a realtime fan-out. Supabase
and Appwrite do. A bare Postgres does not, and for that, `nostos-server` remains
the answer.

### Shape C — a pod on hardware that is allowed to stay awake

Pi, desktop, old laptop; real Postgres, one slot, ADR-0041's iroh transport for
QR-paired dialling with no DNS, TLS or port-forwarding. Technically the cleanest
of the three and already largely designed. **Rejected by the stated constraint:**
it is another device that stays on.

For the record: a phone cannot substitute here, because Postgres does not run on
iOS (fork-per-backend vs. the iOS process sandbox) and PGlite — Postgres 16 in
WASM, <3 MB gzipped — has no replication slot yet
(`electric-sql/pglite#880`, open since 2026-01-21, open PRs). Electric built
PGlite and still keeps a server-side Postgres as the source of truth.

## Recommendation

**Build Shape B2 and make it a first-class sync mode.** It is the only shape
that satisfies every stated constraint at once: a shared database per app, apps
shipped to an agency's clients, all sync compute on the end user's device, no
Nostos server for anybody, nothing centralised that the agency has to own.

It is also not a compromise version of Nostos. The device keeps the engine, the
SQLite store, the read VIEWs, the reactive facade and the durable outbox. What
changes is one port's implementation — which is what ADR-0023 D4 built the seam
for.

**`nostos-server` stops being the entry point and becomes the upgrade path**, for
the cases that actually need it: a bare Postgres with no realtime service,
Nostos-evaluated predicates rather than RLS-plus-topics, LSN-exact replay,
conflation and backpressure under Nostos's control (ADR-0045), or a backend whose
realtime tier costs more than a $2 machine.

### The work

1. **A `ChangeSource` seam in the client.** `nostos-client` today speaks only the
   `/sync` WebSocket (`client.rs`), with `iroh_dial.rs` as the precedent for a
   second dial path. Factor the frame source out behind a trait; `ApplyEngine`,
   storage, views and outbox are untouched.
2. **A Supabase adapter behind it:** Realtime private channel → decode
   `realtime.broadcast_changes()` payloads → `RowOp`; PostgREST catch-up query
   from the watermark; outbox drain → PostgREST upsert/delete.
3. **Watermark checkpointing** alongside the existing LSN checkpoint in
   `cairn_meta`. Per table, since catch-up is per table.
4. **`nostos link --mode direct`** generates the SQL the client's DB needs: the
   trigger per synced table, the broadcast authorization RLS policies, and a
   check that every synced table has a monotonic column and a soft-delete
   column. Refuse to generate for tables that don't — that check is the whole
   product's reliability.
5. **`nostos doctor`** for this mode: trigger present, RLS policies present,
   watermark columns indexed, realtime enabled.
6. **A conformance test both modes pass.** One suite, two change sources —
   because the moment the behaviours diverge silently, the mode becomes a
   support burden instead of a feature.

**Not building:** the embedded replicator (A1 impossible, A2 out of scope),
Shape C, B1 as anything but a documented fallback.

### Worth an ADR

This adds a second sync topology to the public client surface, is hard to
reverse, and trades LSN-exact resume for a watermark. That clears the ADR bar in
`.claude/skills/grill-with-docs`. Not written yet — the decision is the
operator's to take, and this document is the argument for it.

## Open questions

- ~~Shared database or database-per-user?~~ **Answered: shared.**
- Is the `updated_at` + soft-delete schema requirement acceptable to impose on
  client databases? It is the one thing B2 asks of them, and it is not small on
  a legacy schema.
- Does the agency's clients' data ever need Nostos-evaluated predicates, or is
  RLS-plus-topic granularity enough? This is the main fidelity question.
- Verify against a live project: trigger + RLS round trip, catch-up query cost,
  and what a device that was offline 4 days actually receives.

## Sources

- Android: [FGS timeouts](https://developer.android.com/develop/background-work/services/fgs/timeout) ·
  [Android 15 FGS type changes](https://developer.android.com/about/versions/15/changes/foreground-service-types) ·
  [behaviour changes 15](https://developer.android.com/about/versions/15/behavior-changes-15)
- iOS: [Apple DTS on background sockets](https://developer.apple.com/forums/thread/750136)
- Postgres slots: [Mastering Postgres replication slots](https://www.morling.dev/blog/mastering-postgres-replication-slots/) ·
  [WAL slot management at scale](https://streamkap.com/resources-and-guides/postgres-wal-slot-management-production)
- Supabase: [Postgres Changes](https://supabase.com/docs/guides/realtime/postgres-changes) ·
  [Realtime limits](https://supabase.com/docs/guides/realtime/limits)
- Brick: [Supabase blog](https://supabase.com/blog/offline-first-flutter-apps) ·
  [brick_offline_first_with_supabase](https://pub.dev/packages/brick_offline_first_with_supabase)
- PGlite: [about](https://pglite.dev/docs/about) · [issue #880](https://github.com/electric-sql/pglite/issues/880)
- Supabase Realtime, fetched 2026-09-22:
  [subscribing to database changes](https://supabase.com/docs/guides/realtime/subscribing-to-database-changes)
  ("Broadcast … is the recommended method for scalability and security") ·
  [Broadcast](https://supabase.com/docs/guides/realtime/broadcast)
  (`realtime.broadcast_changes()`, one publication on `realtime.messages`,
  ~3-day partition retention) ·
  [Postgres Changes](https://supabase.com/docs/guides/realtime/postgres-changes)
  (per-subscriber authorization, single-threaded, ~3,000-subscriber guidance)
- Pricing, fetched 2026-09-22: [Fly.io](https://fly.io/docs/about/pricing/) ·
  [Supabase](https://supabase.com/pricing)
