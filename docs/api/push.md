# Push notifications (ADR-0037)

Server-side push for nostos: OS-level wake + Live Activity updates, derived
from the same predicate pass that feeds WebSocket fan-out. Push is a hint —
the client's durable LSN checkpoint is the correctness mechanism; a missed or
stale push loses nothing.

Everything here is configured server-side; no SDK wire protocol changes.

## Token registration (REST)

Both routes use the same JWT bearer auth as `/sync`; `tenant_id`/`account_id`
are stamped server-side from the authenticated principal and a client-sent
one is rejected (ADR-0018 discipline).

| Route | Purpose |
|---|---|
| `POST /push-tokens` | body `{"platform":"…","token":"…"}` → `204` |
| `DELETE /push-tokens/{token}` | sign-out deregistration → `204` (owner-scoped, idempotent) |

`platform` is one of:

| Platform | Carries |
|---|---|
| `fcm` | an FCM HTTP v1 device token (or `fid`) |
| `apns` | an APNs device token (64-hex class) |
| `webpush` | a Web Push subscription endpoint |
| `apns-liveactivity` | an ActivityKit push token — **experimental**, see below |

## Server configuration

Rails come from env (`nostos-server` refuses to start on a half-configured
rail):

| Rail | Env |
|---|---|
| FCM | `NOSTOS_FCM_CREDENTIALS_JSON` (service-account JSON) |
| APNs | `NOSTOS_APNS_KEY_P8` (p8 PEM or path), `NOSTOS_APNS_KEY_ID`, `NOSTOS_APNS_TEAM_ID`, `NOSTOS_APNS_BUNDLE_ID`, optional `NOSTOS_APNS_SANDBOX=1` |
| Web Push | `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY`, `NOSTOS_WEBPUSH_VAPID_SUBJECT` |
| Coalescer | `NOSTOS_PUSH_DEBOUNCE_MS` (default 2000 — bursts to one account collapse to one push per window) |

### `NOSTOS_PUSH_TABLES`

`;`-separated per-table entries; each is one of:

- `table` — silent doorbell (content-free wake; payload is at most
  `{table, lsn}` — row data never transits a vendor),
- `table:silent` — the same, explicit,
- `table:visible:<title>:<body>` — a visible notification; `{col}`
  statically interpolates the triggering row's column value (a missing column
  → empty string; no expression language),
- `table:visible@<route>:<title>:<body>` — the same, plus the in-app
  destination a tap should open. `<route>` takes `{col}` too and must start
  with `/`; it ships as the payload key `cairn_route` (ADR-0037 §2a). `@`
  rather than one more `:` because the body is the greedy remainder. Works
  on `action` entries as well
  (`table:action@<route>:<category>:<title>:<body>`); a silent doorbell
  rejects it at startup — a wake-up carries no routing keys,
- `table:liveactivity:<json>` — **experimental** Live Activity updates, see
  below; `<json>` is a JSON object whose string leaves may carry `{col}`
  placeholders.

```
NOSTOS_PUSH_TABLES='tasks;orders:visible@/orders/{id}:New order:Order {id} placed;deliveries:liveactivity:{"status":"{status}","eta_min":"{eta_min}"}'
```

Startup fails on a typo'd table, an unknown mode, a malformed liveactivity
template, or a duplicate entry — a table silently not pushing is the failure
mode this refuses to allow. Colons cannot appear in title/body and semicolons
cannot appear anywhere in an entry (they separate entries — including inside
a liveactivity JSON template).

### Routing keys — where a tap lands

A visible push carries an optional string→string `data` map that reaches the
app on tap: APNs puts it next to `aps` (which is what `userInfo` returns),
FCM in `message.data`, Web Push in a `data` object inside the encrypted
payload. `NOSTOS_PUSH_TABLES` sets the one key `cairn_route` via `@route`
above; `nostos-pushd`'s `POST /v1/send` takes the whole map:

```json
{"token": "…", "payload": {"visible": {
  "title": "Order shipped",
  "body": "Order 983979e8 is on its way",
  "category": "order_status",
  "data": {"cairn_route": "/orders/983979e8", "order_id": "983979e8"}
}}}
```

Refused with a 400 (and at startup, for the config path): keys a rail would
eat — `aps`, FCM's `from` / `message_type` / `notification` / `google.*` /
`gcm.*`, and nostos's own `title`, `body`, `category`, `table`, `lsn` — or a
map over 1024 serialized bytes (APNs and FCM cap the whole payload at 4096).

Silent doorbells carry no `data`: the payload stays `{table, lsn}`. And the
map is plaintext at the vendor — put identifiers in it, not the row.

Tables listed here also doorbell the tenant's fully-offline accounts
(`NOSTOS_TENANT_COLUMN` targeting); every other table only doorbells via
matched sessions.

## Live Activities — EXPERIMENTAL

> **Experimental** (ADR-0037 §5, plan task 6.4). The known ceiling is
> ActivityKit's token-rotation bookkeeping: push tokens are per-activity and
> rotate mid-flight, so the app MUST re-register on every
> `pushTokenUpdates` emission. If it doesn't, updates silently stop at the
> first rotation. ADR-0033 discipline: prove it in your deploy before you
> depend on it.

What the server does for a `table:liveactivity:{…}` entry when a matching
row commits:

- The template's string leaves interpolate the row's columns (same static
  `{col}` rules) and ship as the ActivityKit `content-state`, sent with
  `apns-push-type: liveactivity`, `apns-topic:
  <bundle>.push-type.liveactivity`, `apns-priority: 5` (the budget-free
  update tier — priority 10 counts against the device's hourly update
  budget) and `{"aps":{"timestamp":<now>,"event":"update",
  "content-state":{…}}}`. `timestamp` is Apple's newest-wins anchor;
  `apns-collapse-id` (the table name) supersedes in-flight updates per
  (device, subscription) like the other rails, and `apns-expiration`
  bounds staleness at ~15 minutes — a late update renders outdated Lock
  Screen state, which is worse than no update.
- Only tokens registered with platform `apns-liveactivity` receive the
  state update. Ordinary device tokens of the same account still get the
  silent doorbell — the activity update repaints the Lock Screen, it does
  not move the device's LSN.
- ActivityKit tokens are never doorbelled (they cannot wake the app).
- Updates are suppressed while the account has a live session (the
  foregrounded app can update its own activity); the coalescer re-checks
  presence at send time.

### App-side wiring (Swift)

Start the activity with `pushType: .token`, register the token with nostos,
and re-register on every rotation (delete the superseded token — the
registry keys rows by token, and a stale row keeps receiving dead sends
until APNs prunes it with a 410):

```swift
import ActivityKit

func startActivity(deliveryId: String) throws {
    let attributes = DeliveryAttributes(id: deliveryId)
    let state = DeliveryAttributes.ContentState(status: "scheduled", etaMin: 0)
    let activity = try Activity.request(
        attributes: attributes,
        content: .init(state: state, staleDate: nil),
        pushType: .token
    )

    // ActivityKit mints/rotates the token asynchronously — the for-await
    // loop sees the first token AND every rotation.
    Task {
        for await token in activity.pushTokenUpdates {
            guard let token else { continue }
            let hex = token.map { String(format: "%02x", $0) }.joined()
            try await register(token: hex)     // POST /push-tokens below
        }
    }
}

func register(token: String) async throws {
    var req = URLRequest(url: URL(string: "\(server)/push-tokens")!)
    req.httpMethod = "POST"
    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
    req.setValue("Bearer \(jwt)", forHTTPHeaderField: "Authorization")
    req.httpBody = try JSONEncoder().encode(
        ["platform": "apns-liveactivity", "token": token])
    let (_, resp) = try await URLSession.shared.data(for: req)
    precondition((resp as! HTTPURLResponse).statusCode == 204)
}
```

On rotation, `DELETE /push-tokens/{old-token}` the superseded hex token
(owner-scoped; a 204 even if it already pruned). On activity end / sign-out,
`DELETE` the current one.

The template's `content-state` must decode into the activity's
`ActivityAttributes.ContentState` type with default encoding strategies
(custom `JSONEncoder` strategies fail system-side, per Apple's docs). For the
example config above the struct is:

```swift
struct DeliveryAttributes: ActivityAttributes {
    public struct ContentState: Codable, Hashable {
        var status: String
        var etaMin: String   // {col} interpolation yields strings
    }
    let id: String
}
```

### Known limits (v1)

- `update` events only — `start` and `end` ActivityKit pushes need a
  distinct payload shape (`attributes`, `dismissal-date`); start the
  activity from the app. On-disk row deletion does not end an activity.
- The optional Apple `stale-date` field is deliberately not set: nostos
  cannot distinguish "data legitimately quiet" from "data stale" —
  premature dimming is worse than omitting it. The 15-minute
  `apns-expiration` bounds delivered-staleness instead.
- Per-row collapse key is the table name: many concurrent activities of the
  same table on one device supersede each other in flight. `timestamp`
  ordering still renders the newest state; if you need independent
  per-activity keys, split tables or wait for keyed templates.
- `{col}` values stringify — numbers arrive as `"12"`, not `12`. Type the
  `ContentState` fields as `String` (or parse them app-side).

## Related

- Decision record: [`../adr/0037-sync-aware-push-notifications.md`](../adr/0037-sync-aware-push-notifications.md)
- Implementation plan: `docs/plans/nostos-push-notifications-implementation.md` (removed in cleanup; see git history)
- Security model (token trust boundary): [`../SECURITY-MODEL.md`](../SECURITY-MODEL.md)
