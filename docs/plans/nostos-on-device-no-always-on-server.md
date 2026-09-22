# Nostos with no always-on server — the on-device shapes and what each costs

**Date:** 2026-09-22. **Status:** exploration, no decision taken.
**Prompted by:** the operator's constraint, stated plainly — *"I just do not want
to involve another device that stays on all the time for Nostos to work."*

The constraint is legitimate and the current answer ("run nostos-server on a VM
or a spare box") is a real adoption tax. This document maps the shapes that
satisfy the constraint, and is honest about which one dies where. The short
answer is that it hinges on one thing only: whether every install reads from
**one shared database** or from **its own**.

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

### Shape B — `nostos lite`, direct to the backend's own API

No replication at all: the client speaks the backend's REST/realtime surface
(PostgREST + Realtime on Supabase) and keeps Nostos's local SQLite, outbox and
reactive facade. Zero slots, zero hosting.

This is the `brick_offline_first_with_supabase` topology. Its ceiling is the
backend's, not ours: Supabase authorises **every** Postgres-Changes event
against **every** subscriber, on a **single thread**, and their docs tell you to
leave Postgres Changes past **~3,000 concurrent subscribers**. Delivered
messages are also metered — one write to 500 subscribers is 500 billed
messages.

**What it costs us:** server-authoritative scoping moves to RLS, durable resume
degrades from "replay since LSN X" to "retry queued requests", and the
write-back pitch ("no `uploadData()` endpoint") stops being true.

**What it buys:** the onboarding story the project currently lacks — zero
infrastructure to start, and an upgrade trigger with a number on it.

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

**A2 is the answer to the operator's question**, and it should be built and
documented as a first-class deployment shape, not a footnote to A1. It is the
only shape that removes the always-on server without giving anything up:
per-install router, full replication semantics, no second device, no ceiling
that grows with the user count.

The order:

- **A2 — "own your database"** — embedded router, user-supplied connection
  string, one slot per database. Build the target support first (step 2 below);
  everything else follows.
- **Shape B as the zero-setup onboarding path** — no database to bring at all,
  with "~3,000 subscribers, or the moment you need server-authoritative
  scoping" as the documented trigger to move. *The upgrade path is the product.*
- **A1 is the shape that needs `nostos-server`**, and it is also the shape most
  commercial apps are. Keep it, and say why it exists in one line:

> Postgres offers ~10 replication slots. If every install reads from **one
> shared** database, something must multiplex that single slot into many
> sessions — that is `nostos-server`. If each user owns their own database, you
> don't need it.

That sentence belongs in the README. It is a choice about the *data topology*,
not about how much compute a phone has.

## If Shape A goes ahead — the actual work

1. `nostos-infra` behind an `embedded` feature in `sdk/nostos_flutter/rust`
   (today it depends only on `nostos-client`, `nostos-core`, `nostos-domain`).
2. Verify `tokio-postgres` + rustls on `aarch64-apple-ios` and
   `aarch64-linux-android`. Unproven; this is the first thing to test.
3. Per-install slot naming, and a reaper for slots whose device never returns.
   Required for A1; cheap insurance for A2, where the abandoned slot is the
   owner's own.
4. Credential story: a user-supplied connection string, entered or scanned at
   setup and kept in the platform keystore. Never a `REPLICATION` role baked
   into the bundle — `nostos doctor` should fail loudly if it finds one.
5. Foreground-only sync loop plus resume-on-launch; no background service. Push
   (ADR-0037) is the only wake mechanism.

## Open questions for the operator

- **The fork, first:** does the flagship story assume one shared database per
  app (A1) or one database per user (A2)? Every other answer depends on it.
- If A2: who provisions the per-user database — the user by hand, or Nostos via
  a provider API? The second is a product; the first is a paragraph of docs.
- Does Shape B get built before or after the Flutter+Supabase launch bar?
- Is `max_replication_slots` on Supabase's free tier even raisable? Only
  matters for A1. Unverified;
  `docs/plans/flutter-supabase-plug-and-play-launch.md` W0 flags the same gap.

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
