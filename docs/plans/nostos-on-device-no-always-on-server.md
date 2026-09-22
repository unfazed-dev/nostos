# Nostos with no always-on server — the on-device shapes and what each costs

**Date:** 2026-09-22. **Status:** exploration, no decision taken.
**Prompted by:** the operator's constraint, stated plainly — *"I just do not want
to involve another device that stays on all the time for Nostos to work."*

The constraint is legitimate and the current answer ("run nostos-server on a VM
or a spare box") is a real adoption tax. This document maps the shapes that
satisfy the constraint, and is honest about which one dies where.

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

### Shape A — embedded replicator (one slot per install)

The app's Rust lib links `nostos-infra` with the `pg` feature and holds the
logical-replication connection itself.

**Works:** no second device, no hosting bill, full Nostos semantics (predicates,
LSN checkpoints, op-log resume, write-back).

**Ceiling — and it is a hard one:** every *install* holds its own replication
slot against the Postgres. `max_replication_slots` defaults to **10**, and the
standing advice is to keep it tight rather than generous. Three independent
failure modes follow:

1. **Slot count scales with installs.** 1,000 users on 1,000 phones is 1,000
   slots on one database. Not a per-user problem — a per-*install* one.
2. **One laggard pins WAL for everyone.** Each slot retains WAL from its own
   confirmed position, so a single phone offline for days sets the retention
   floor for the whole database. Disk fills, writes halt. This is the ADR-0043
   class, multiplied by the install count.
3. **Uninstall leaves an abandoned slot.** Nothing on the device can clean up
   after itself once the app is gone. `max_slot_wal_keep_size` bounds the
   damage by invalidating the slot, at the cost of a full resync for any device
   that was merely asleep.

It also puts a `REPLICATION`-privileged credential inside the app bundle.

**Therefore:** Shape A is sound for **one owner, few devices** — your phone,
your Postgres, single digits of installs, `max_replication_slots` raised to
match. It is not a shape to ship to strangers.

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

Ship **both ends, honestly labelled**, and do not make either the default:

- **Shape A as "personal mode"** — documented with the slot arithmetic in the
  first paragraph, not a footnote. Target: the developer's own device, a family
  app, a self-hosted-per-user deployment.
- **Shape B as the onboarding path** — start with no infrastructure, and make
  "~3,000 subscribers, or the moment you need server-authoritative scoping" the
  documented trigger to point at a real `nostos-server`. *The upgrade path is the
  product.*

Neither removes the always-on server for a multi-tenant product. That is not a
gap in Nostos: Postgres offers ~10 replication slots and a shipped app has
thousands of installs, so **something must multiplex one slot into many
sessions.** That is nostos-server's entire reason to exist, and it should be said
that way in the README.

## If Shape A goes ahead — the actual work

1. `nostos-infra` behind an `embedded` feature in `sdk/nostos_flutter/rust`
   (today it depends only on `nostos-client`, `nostos-core`, `nostos-domain`).
2. Verify `tokio-postgres` + rustls on `aarch64-apple-ios` and
   `aarch64-linux-android`. Unproven; this is the first thing to test.
3. Per-install slot naming, and a reaper for slots whose device never returns.
4. Credential story: a `REPLICATION` role in an app bundle is only acceptable
   when the database owner and the device owner are the same person. Enforce
   that in documentation *and* in `nostos doctor`.
5. Foreground-only sync loop plus resume-on-launch; no background service. Push
   (ADR-0037) is the only wake mechanism.

## Open questions for the operator

- Is "personal mode" a product, or a documented power-user path?
- Does Shape B get built before or after the Flutter+Supabase launch bar?
- Is `max_replication_slots` on Supabase's free tier even raisable? Unverified;
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
