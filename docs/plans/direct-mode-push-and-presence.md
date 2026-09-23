# Direct mode: push notifications and presence, from the platform docs

Companion to `direct-mode-sync-protocol.md`. Everything here is sourced from
official docs fetched 2026-09-22 — links at the bottom, and every hard limit in
this document is quoted rather than remembered.

## The answer first

**A design where sync is always correct and push is always best-effort is
achievable and I am certain of it. A design where a silent background wake
always arrives is not, on any platform, in any sync product, including the one
Nostos ships today.**

Those are not the same sentence, and the difference is the whole design:

- **Correctness never waits on a push.** The snapshot horizon already makes
  every pull resumable and lossless. A doorbell that never arrives costs
  *staleness until next open*, not a missed row. This is already true of
  nostos-server's doorbell (ADR-0037) — direct mode inherits the property, it
  does not introduce it.
- **Two of the platform ceilings are absolute**, straight from the vendors:
  - Apple DTS, asked how to wake a force-quit app in the background: **"The
    short answer to all of these is *You can't*."** And for background pushes
    generally: *"the system doesn't guarantee their delivery… the system may
    throttle the delivery of background notifications if the total number
    becomes excessive."* The budget is **device-wide across all apps**, and
    background push is not delivered at all if the user has not launched the
    app for a while.
  - Chrome/Edge reject `userVisibleOnly: false` on the open web. Fail to show a
    notification on a `userVisibleOnly: true` subscription and Chrome shows
    *"This site has been updated in the background"* for you; repeat offenders
    lose the subscription. The Budget API that was meant to license silent push
    never shipped and `navigator.budget` is gone.

Anyone who tells you their offline-sync product reliably background-wakes a
force-quit iOS app is either using a visible notification or is wrong. So the
design below makes the reliable paths carry the load and treats the unreliable
ones as pure upside.

## The wake ladder — what actually happens, per state

| device state | mechanism | reliability | source |
|---|---|---|---|
| app foreground / web tab open | Realtime broadcast on the already-open WebSocket | **deterministic** | Realtime docs |
| Android backgrounded or OS-terminated | FCM data message, `collapse_key`, TTL | **reliable, possibly delayed** — Doze holds normal-priority messages until the device wakes | FCM priority docs |
| iOS backgrounded, app used regularly, **not** force-quit | APNs background push via FCM: `apns-priority: 5`, `apns-push-type: background`, `content-available: 1` | **best-effort, a few per hour device-wide, may be none at all** | Apple, *Pushing background updates* |
| iOS force-quit by the user | nothing silent. Only a **visible alert** push gets through | **zero** | Apple DTS |
| web, tab closed | Web Push, but it **must** render a notification | **zero for silent sync** | Chrome / MDN / W3C |
| any platform, next app open | WS connects → `rpc/pull` from the stored horizon | **deterministic** | this repo |

Row 1 and the last row are the two rows that matter. Rows 2–5 are accelerators.

**Three traps worth naming**, because each one produces a bug report that says
"it worked yesterday":

1. **Xcode relaxes the throttle.** Apple: while debugging, *"the rules for
   delivery of silent pushes are relaxed"*. This is the single most common
   source of "works from Xcode, fails from TestFlight". Never validate the iOS
   wake path from a debug build.
2. **Force-quit ≠ terminated.** OS termination (memory pressure, watchdog,
   reboot) leaves background delivery intact. Only a user swipe in the app
   switcher sets the flag, and only a relaunch or a device restart clears it.
3. **FCM rejects high priority for iOS data messages.** *"when sending data
   messages to Apple devices, the priority must be set to 5… messages sent with
   high priority are rejected by the FCM backend with the error
   `INVALID_ARGUMENT`"*.

## The online doorbell: `realtime.send()` from the trigger

Better than what the sync plan first proposed, and it is Supabase's own
recommendation:

> `postgres_changes` should be avoided due to scalability limitations — use
> `broadcast` with database triggers (`realtime.broadcast_changes`) for all
> database change notifications.

Why it fits exactly:

- **Realtime reads the WAL** via a publication on `realtime.messages`, so a
  broadcast is emitted **post-commit by construction**. The "doorbell for a row
  the device cannot yet pull" failure mode is structurally impossible. This
  closes the atomicity W0 check from the sync plan.
- **`realtime.send()` is trigger-safe by design** — it catches its own
  exceptions and reports them via `pg_notify`, so a Realtime outage cannot fail
  the application's write transaction.
- **It scales on writes, not subscribers.** `postgres_changes` authorizes every
  event per subscriber (100 subscribers = 100 authorization checks) on a single
  ordering thread; Supabase caps its recommendation at ~3,000 concurrent
  subscribers and points at Broadcast past that.
- **`realtime.messages` is daily-partitioned**, so retention is a partition
  drop, not a `DELETE`.

Requirements that come with it:

- **Private channels are mandatory**: `realtime.broadcast_changes()` uses a
  private channel and needs RLS policies on `realtime.messages`. Policies
  discriminate on the `extension` column (`broadcast` / `presence`) and scope
  with the `realtime.topic()` helper. Realtime runs the policy query and rolls
  it back — nothing is persisted.
- **Public/private must match on both ends.** A public broadcast never reaches a
  private channel and vice versa. Same topic name, different privacy = two
  distinct channels.
- **Turn off "Allow public access"** in Realtime Settings, or anyone holding the
  anon key can subscribe to and broadcast on any public channel with no policy
  check at all. With it off, non-private joins are rejected `PrivateOnly`.
- Policies are cached per connection and refreshed on join and on
  `access_token`; a complex policy shows up as connection latency and, if the
  authorization pool is undersized, as `IncreaseConnectionPool` with dropped
  broadcasts and failed presence calls.

**Also available and useful:** a REST doorbell, no WebSocket needed —
`POST /realtime/v1/api/broadcast/<topic>/events/<event>` with an `apikey`
header and `?private=true`, plus a batch form `POST /realtime/v1/api/broadcast`
taking a `messages[]` array. Dart has `channel.httpSend()`. This is how the push
Edge Function can also ring the socket doorbell, and how `nostos doctor` can
inject a test doorbell.

**Belt and braces we do not need but get anyway:** Broadcast Replay lets a
private channel ask for missed database-broadcast messages with
`since` (epoch ms) and `limit` (max 25), retained in daily partitions for ≥72h
and ≤4 days. Nostos's horizon already makes replay unnecessary — noted so nobody
builds it twice.

## The push path: trigger → `pg_net` → Edge Function → FCM

```
write txn
  ├─ cairn.changes insert (trigger)
  ├─ realtime.send(...)        → WAL → Realtime → online devices   [instant]
  └─ net.http_post(...)        → queued, NOT SENT until COMMIT
                                    ↓ (post-commit, background worker)
                              Edge Function (service_role + FCM secret)
                                    ↓ presence filter
                              FCM HTTP v1 → APNs/FCM → offline devices
```

### `pg_net` gives us commit atomicity for free

From the official page, on every one of `http_get`/`http_post`/`http_delete`:
**"HTTP requests are not started until the transaction is committed."** The call
inserts a row into `net.http_request_queue` and wakes a libcurl background
worker. A queue row is an ordinary row, so **`ROLLBACK` discards it and no
request is ever sent.**

That is both halves of the atomicity problem solved by the extension, not by us:
no doorbell before the data is visible, no doorbell for a write that aborted.
No two-phase commit, no outbox poller.

**Its documented limits, all of which bind this design:**

| limit | value | consequence here |
|---|---|---|
| throughput | *"configured to reliably execute up to 200 requests per second… Increasing the rate can introduce instability"* | **one `net.http_post` per write transaction, not per device.** The function fans out to tokens. 200 write-txns/sec of push-worthy changes is the ceiling. |
| durability | requests and responses live in **unlogged tables**, *"not preserved during a crash or unclean shutdown"* | a Postgres crash in the send window loses that doorbell → staleness, not data loss. The pull-on-open path is the recovery. |
| batch | `pg_net.batch_size` default 200 rows per worker wake | a transaction that queues thousands hands them over at once |
| methods | POST with JSON only; no PATCH/PUT | fine — the function takes a JSON body |
| observability | responses kept 6h in `net._http_response` | `select * from net._http_response where status_code >= 400 or error_msg is not null` is the `nostos doctor` query |
| reconfigurable | only on pg_net **v0.12.0+** | `nostos doctor` should report the version |

⚠️ **Never put a trigger on `net._http_response` or `net.http_request_queue`** —
if it fails or calls a pg_net function it can loop infinitely. Concretely:
`nostos link` must refuse to install change-log triggers on the `net` schema.

### The credential lives in the function, and that is the whole point

The Edge Function holds the FCM service-account JSON as a Supabase secret and
mints an OAuth2 token in-process, then POSTs to
`https://fcm.googleapis.com/v1/projects/{project-id}/messages:send`.

**Use FCM as the APNs proxy.** Upload the APNs `.p8` auth key to Firebase once
and the function needs exactly one credential instead of two, with no HTTP/2
APNs client to get right inside Deno. Firebase requires the APNs key upload
before FCM works on iOS anyway.

This is *not* the shared-replication-role problem wearing a hat. The distinction is
where the secret sits, not whether one exists:

| | holder | reachable by an attacker with an APK? |
|---|---|---|
| a shared replication role (`replication bypassrls`) | would have to be the device | **yes** — and it reads every row regardless of policy |
| FCM service account | Supabase secret, inside the function | **no** |
| `service_role` (function reads `cairn.push_tokens`) | Supabase secret, inside the function | **no** |

The function using `service_role` to read tokens bypasses RLS, which is correct
and safe: it is server-side code the developer deploys, and the key never leaves
Supabase. Direct mode's claim is *"no server the developer operates"* — a
scale-to-zero function is not a server you operate.

### Payload, with the platform quirks already applied

```json
{
  "message": {
    "token": "<registration token>",
    "data": { "t": "orders", "h": "<horizon as string>" },
    "android": { "priority": "normal", "collapse_key": "<nostos key>", "ttl": "3600s" },
    "apns": {
      "headers": {
        "apns-priority": "5",
        "apns-push-type": "background",
        "apns-collapse-id": "<nostos key>"
      },
      "payload": { "aps": { "content-available": 1 } }
    }
  }
}
```

Four things in there are load-bearing:

1. **`data` values must be strings.** A number yields
   `Invalid value at 'message.data[0].value' (TYPE_STRING)`. The horizon is an
   opaque string in the sync plan already — same rule, two reasons.
2. **`priority: normal` on Android for a silent sync.** High priority is for
   messages that *"generally should result in user interaction"*; abuse shows up
   as `priorityLowered` in the FCM dashboard.
3. **`collapse_key` is a data-message tool** — notification messages ignore it.
   Nostos's `default_collapse_key(tenant, token)` (`crates/nostos-push/src/rail.rs:182`)
   is a **pure function**, so the Edge Function can reproduce it byte-for-byte
   and get ADR-0038's asserted behaviour: 20 sends to one target ⇒ 1 push.
4. **Only four distinct collapse keys are stored per device**, evicted
   arbitrarily past that. One key per (tenant, token) keeps us at 1. Do not make
   the key per-table.

**The Android overflow path is already Nostos's re-snapshot path.** Non-collapsible
messages cap at 100 stored; past that FCM discards all of them and sends
`onDeletedMessages`, which *"the app typically handles by requesting a full
sync"*. That is exactly "device offline past the retention window" from step 8 of
the sync plan — one handler, two callers.

**FCM quotas:** `sendEachForMulticast` takes **500 tokens max** and fans out to
500 individual HTTP requests; HTTP v1 allots **600K quota tokens per 1-minute
bucket**, and Firebase explicitly warns against bursting at the window start.
Chunk at 500 with jittered backoff; 429 → honour `retry-after` (default 60s);
400/401/403/404 → do not retry; 5xx → exponential backoff.

**One forward-looking note:** FCM has deprecated `token` in favour of Firebase
Installation IDs (`fid`/`fids`, `FidMessage`, `FidMulticastMessage`); `tokens`
still accepts FIDs during migration, and if both are given tokens go first.
`crates/nostos-push/src/rail.rs:56` already knows this. `cairn.push_tokens`
should carry the target as an opaque string with a `kind` discriminator so the
migration is a data change, not a schema change.

## Presence: two different jobs, and conflating them is the mistake

### Job A — user-visible presence ("Alice is online")

**Use Supabase Realtime Presence. Direct mode gets this for free, and better
than nostos-server has it today** — nostos's `SessionStore` tracks sessions for
fan-out decisions, not for UI.

It is purpose-built: each client publishes a payload under a presence key,
Realtime keeps the merged view, and `join`/`leave`/`sync` events fire. State is
**persisted in the channel**, so a new joiner immediately sees who is there
without waiting for anyone to re-announce. Ungraceful disconnect is handled for
you by the WebSocket heartbeat — the thing a hand-rolled presence table always
gets wrong. Authorization is an RLS policy for `insert` where
`realtime.messages.extension = 'presence'`.

Its documented limits:

| | Free | Pro | Pro (no cap) / Team |
|---|---|---|---|
| concurrent connections | 200 | 500 | **10,000** |
| messages / sec | 100 | 500 | 2,500 |
| presence messages / sec | 20 | 50 | 1,000 |
| presence keys per object | 10 | 10 | 10 |
| **presence calls per client / 30s** | **5** | **5** | **5** |
| channels per connection | 100 | 100 | 100 |

Two rules follow. **Call `track()` once, after `SUBSCRIBED`** — the 5-per-30s
cap is identical on every plan including Enterprise, and blowing it earns
`ClientPresenceRateLimitReached` and a closed channel. And **never put anything
fast-moving in a presence payload**; Supabase says use Broadcast for that.

**The number that bounds direct mode as a whole: 10,000 concurrent Realtime
connections.** That is the device ceiling for one shared database, and it is
Supabase's limit, not Nostos's. Worth stating plainly in the README rather than
discovered by a client at 10,001.

### Job B — push suppression ("don't burn the iOS budget on a live device")

**Realtime Presence cannot do this job.** Presence is in-memory in the Realtime
server, WebSocket-only, and **there is no admin REST endpoint to read presence
state**. A Postgres trigger can read SQL and nothing else. Confirmed against the
docs; the alternatives are a server that joins the channel as a client (absurd
for a scale-to-zero function, and it would show up as a channel member) or
mirroring into Postgres.

So mirror into Postgres, and make it cost nothing:

```sql
-- excluded from the change-log triggers: it must never feed cairn.changes
create table cairn.device_presence (
  device_id   text primary key,
  scope       text not null,
  last_seen   timestamptz not null default now()
);
```

**`cairn.pull()` stamps `last_seen` as a side effect.** It is already `volatile`
and already called via POST on every doorbell and every reconnect, so an
actively-syncing device has a fresh `last_seen` **for zero extra round trips**.
Idle-but-connected devices need one cheap `nostos.heartbeat()` on a 60s timer
while the socket is open — 1 request/minute/device, ~17 req/s at 1,000
concurrent devices, which PostgREST does not notice.

The suppression predicate is then just SQL the function can run:

```sql
where p.last_seen < now() - interval '90 seconds'
```

**Scope it to iOS first.** iOS is the only platform with a scarce, device-wide,
cross-app background budget, so it is the only platform where a wasted send has
a real cost. Android sends are effectively free and web sends are visible
anyway. Measure before extending — the project rule.

⚠️ **`cairn.device_presence` must be excluded from the change-log triggers.**
A heartbeat that writes to `cairn.changes` is a write-amplification feedback
loop that doorbells every device once a minute forever. `nostos link` should
refuse to instrument it, and `nostos doctor` should assert it is not instrumented.

### Why the suppression is an optimization and not a correctness device

A device that is online receives **both** the Realtime broadcast and the push.
That must be harmless, and it is: both say "pull", the pull runs from the stored
horizon, and if nothing changed it returns zero rows. Duplicate doorbells are
already idempotent by construction. Suppression exists only to conserve Apple's
budget, so a stale `last_seen` costs one wasted send, never a wrong result.

## What this means for nostos-server mode

**The iOS and web ceilings are identical in both modes**, because they are
platform properties. Server mode's advantage is narrower and more honest than
"push works properly":

- `FanOutService` knows, from `SessionStore`, whether a socket is *currently
  connected to it* — no heartbeat table, no 90-second window, exact.
- It can correlate push receipts with LSN progress (ADR-0037's test).
- It evaluates predicates, so it doorbells the matched set rather than a `scope`.

That is a real difference and it should be the pitch. "Push only works on
nostos-server" would not be true.

## What to verify, and with what

`apps/atlet/flutter` already owns the whole rail. Reuse it:

| check | harness | note |
|---|---|---|
| doorbell post-commit, never pre-commit | `psql`: open txn, insert, observe nothing, commit, observe broadcast | the WAL path makes this structural; test it anyway |
| rollback sends nothing | same, with `ROLLBACK` | asserts the pg_net queue-row property |
| 20 changes ⇒ 1 push | `tool/push_smoke.sh`, swapping the server metric for the function log | ADR-0038's test-that-matters, unchanged device-side |
| Android background wake | `tool/push_smoke.sh` on the emulator | already automated; `10.0.2.2` becomes the function URL |
| **iOS background wake** | **physical device, release build, no debugger** | `PUSH_SMOKE_DEVICE=ios` exists. A debug build proves nothing here |
| iOS force-quit | physical device | **expected to fail.** Encode the expectation so nobody "fixes" it |
| web notification renders | `atlet-push-sw.js` already falls back to a visible notification for payload-less and doorbell pushes | this is why Chrome does not revoke the subscription |
| presence join/leave | Realtime Inspector on the topic, then `track()` from the app | Inspector cannot call `track()` for you |
| suppression | two devices, one heartbeating, assert one push | the `last_seen` window |
| `pg_net` failures visible | `select * from net._http_response where status_code >= 400` | wire into `nostos doctor --mode direct` |

**Atlet's web arm already encodes two of these findings**, written before this
research: `web/atlet-push-sw.js` always renders *something* ("a data-less push
is otherwise invisible and indistinguishable from a lost one"), and
`lib/push/push_pilot_web.dart` states "while the page is open the live sync
socket carries the update (the offline gate suppresses pushes by design)". The
tiered ladder above is the design that file already assumed.

## Open questions this research did not close

1. **Edge Function cold-start latency** sets the floor on wake-to-data for a
   backgrounded app. Unmeasured; measure before quoting a number.
2. **Does a foreground-app `content-available` push consume the device-wide
   background budget?** Apple says priority-5 messages are throttled
   *"regardless of payload"* but frames the budget as a background-activity
   budget. Unresolved in the docs — which is itself the argument for
   suppression rather than against it.
3. **`pg_net` 200 req/s versus a bulk import.** A migration touching 10k rows in
   one transaction queues one doorbell (fine), but 10k separate transactions
   queue 10k (not fine). Needs a debounce in the trigger — likely the same
   coalescing window ADR-0038 already implements, moved into SQL.

## Sources (fetched 2026-09-22)

- Apple, [Pushing background updates to your App](https://developer.apple.com/documentation/usernotifications/pushing-background-updates-to-your-app)
  · [iOS Background Execution Limits](https://developer.apple.com/forums/thread/685525)
  · [Background fetch after force quit](https://developer.apple.com/forums/thread/666149)
- Firebase, [Send a message using FCM HTTP v1](https://firebase.google.com/docs/cloud-messaging/send/v1-api)
  · [Message priority](https://firebase.google.com/docs/cloud-messaging/customize-messages/setting-message-priority)
  · [Android message priority](https://firebase.google.com/docs/cloud-messaging/android-message-priority)
  · [Non-collapsible and collapsible messages](https://firebase.google.com/docs/cloud-messaging/customize-messages/collapsible-message-types)
  · [Best practices at scale](https://firebase.google.com/docs/cloud-messaging/scale-fcm)
  · [Admin SDK send](https://firebase.google.com/docs/cloud-messaging/send/admin-sdk)
  · [Receive messages in Flutter](https://firebase.google.com/docs/cloud-messaging/flutter/receive-messages)
- Supabase, [Realtime Broadcast](https://supabase.com/docs/guides/realtime/broadcast)
  · [Realtime Presence](https://supabase.com/docs/guides/realtime/presence)
  · [Realtime Authorization](https://supabase.com/docs/guides/realtime/authorization)
  · [Subscribing to Database Changes](https://supabase.com/docs/guides/realtime/subscribing-to-database-changes)
  · [Realtime Limits](https://supabase.com/docs/guides/realtime/quotas)
  · [pg_net](https://supabase.com/docs/guides/database/extensions/pg_net)
  · [Database Webhooks](https://supabase.com/docs/guides/database/webhooks)
  · [Sending Push Notifications](https://supabase.com/docs/guides/functions/examples/push-notifications)
- W3C [Push API](https://www.w3.org/TR/push-api/) · MDN
  [PushSubscriptionOptions.userVisibleOnly](https://developer.mozilla.org/en-US/docs/Web/API/PushSubscriptionOptions/userVisibleOnly)
  · Chrome [Budget API](https://developer.chrome.com/blog/budget-api) (never shipped)
  · Chrome [Use Web Push in extensions](https://developer.chrome.com/docs/extensions/how-to/integrate/web-push)
