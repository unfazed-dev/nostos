# ADR-0037: Sync-aware push notifications — predicate-routed doorbell + visible templates

- **Status:** Accepted (decision ratified via grill session 2026-08-14).
- **Date:** 2026-08-14
- **Research:** `docs/plans/nostos-push-notifications-research-2026-08-14.md` (6
  research passes: engine parity, incumbent pain, rail features, prior art,
  server integration map, per-SDK feasibility).
- **References:** ADR-0007 (defers push, names `PushSink` as the extension
  point), ADR-0013/0018 (write-path trust boundary + tenant force-stamping),
  ADR-0027 (dead-letter discipline), ADR-0029 (sign-out wipe), ADR-0033
  (experimental-behind-flag precedent), `docs/STRATEGY.md:214` (push is a
  wake-up trigger, not a data channel).

## Context

Users need OS-level push when the app is backgrounded or killed. No
partial-replication engine ships this (research §1): ElectricSQL,
Replicache/Zero, Ditto, RxDB, WatermelonDB, InstantDB and Supabase all leave
wake to OS schedulers or DIY FCM glue. Incumbent push tools are
database-blind — FCM topics are public broadcast strings; every app hand-wires
its own row→user→device→token glue (research §2). Device push physically
terminates at APNs/FCM/Web Push; nothing nostos builds replaces those rails, and
nothing any vendor builds escapes iOS silent-push budgets.

Nostos's server already evaluates, per committed event, exactly which session
predicates match (`fan_out`, `crates/nostos-application/src/fanout.rs:196-203`).
That is the asset no push vendor and no sync competitor has.

## Decision

### 1. Predicate-routed push router in the server; no second targeting registry

A `PushNotifier` port (application layer) with APNs / FCM HTTP v1 / Web Push
adapters (infra). Push relevance is **derived from the same predicate pass that
feeds WS fan-out** — the matched-session set is the push candidate set. There is
no parallel push-subscription list to drift from sync state (the anti-pattern
that killed Parse-style targeting and plagues Ably/PubNub channel-push). The
only new state is transport tokens.

**Fully-offline accounts (killed app, no live session):** the matched set alone
cannot doorbell them, but "push arrives when the app is not open" is a v1
requirement. Resolution: for tables listed in push config (`NOSTOS_PUSH_TABLES`),
the router additionally enqueues one per-`(tenant, table)` hint per event batch;
the coalescer expands it to that tenant's registered tokens whose accounts are
offline (presence re-check at send time). Ceiling: offline accounts cannot be
predicate-filtered — over-notification is possible and harmless for silent
doorbells (client syncs, applies nothing); visible pushes are per-table
operator opt-in, so the blast radius is a conscious config choice. The upgrade
path is a durable per-account predicate registry, deliberately rejected for now
because it is exactly the drift-prone second registry this ADR avoids.

### 2. Doorbell semantics — push is a hint, sync is the transport

Data-only payload is at most `{table, lsn}`. Row data never transits
Apple/Google/vendor servers (moat-consistent with ADR-0034's no-blob-bytes
posture). A missed or stale push loses nothing: the client's durable LSN
checkpoint is the correctness mechanism. Because iOS kills apps receive only
*visible* payloads reliably, v1 also carries server-side visible notification
templates: a static per-table title/body with optional single-column
interpolation. No rules engine, no scheduling, no A/B — that is the
marketing-platform layer nostos explicitly does not build.

### 2a. Routing keys on visible pushes — amendment 2026-09-23

A visible push that cannot say *what it is about* dumps the user on the app's
home screen. v1 carried `{title, body, category}`; an app could only infer
the destination from `category`, which is a notification *class*, not a row.
Every incumbent solves this the same way — routing data travels in the
payload's data half — and the Atlet pilot proved the shape locally before
nostos's wire could carry it.

Visible payloads therefore carry an optional `data: map<string,string>`:

| rail | where it lands | why |
|---|---|---|
| APNs | top-level siblings of `aps` | that dictionary *is* `userInfo` on tap (Apple, "Generating a remote notification") |
| FCM | `message.data` (merged with action mode's `title`/`body`/`category`) | `data` is delivered in all three app states; `notification` alone is not |
| Web Push | a `data` object inside the encrypted payload | what a service worker reads as `event.data.json().data` |

String→string, not JSON: FCM's `data` is a `map<string,string>` on the wire,
so any richer type has to be stringified anyway — nostos stringifies nowhere
and the three rails stay byte-comparable.

**Silent payloads do not get this.** A doorbell is `{table, lsn}`; a routing
key on a wake-up is row data looking for an excuse.

**Still not a data channel** (§2 stands). The payload is plaintext at the
vendor, which is exactly Apple's own rule: an identifier the app resolves
locally is fine, the record is not. Enforced, not merely documented —
`validate_data` rejects the keys a rail would eat (`aps`; FCM's `from`,
`message_type`, `notification`, `google.*`, `gcm.*`; nostos's own `title`,
`body`, `category`, `table`, `lsn`) and caps the map at 1024 serialized bytes
against APNs/FCM's 4096-byte ceiling.

Two producers: `nostos-pushd`'s `POST /v1/send` takes the whole map, and
`NOSTOS_PUSH_TABLES` grows one sugar — `orders:visible@/orders/{id}:…` sets
`cairn_route` with the same `{col}` interpolation title/body use. One key
rather than a map there because that config is a colon-delimited string; a
map means JSON-in-env, and nobody has asked for a second key.

### 2b. Visible pushes in direct mode — amendment 2026-09-23

Direct mode shipped the doorbell only: `cairn.wake_absent_devices()` →
`cairn-push` Edge Function → a silent `content-available` push. The Atlet pilot
showed what §2 already said: iOS never wakes a user-quit app for a silent push
and throttles the rest, so order updates only appeared once the app was
reopened.

`cairn.push_templates (table_name, title, body, category, route)` is the
direct-mode `NOSTOS_PUSH_TABLES`, one row per visible table. A change to such a
table posts `{scope, row, title, body, category, route}`. The Edge Function
fills `{col}` and sends the fcm.rs shapes: `action` when category is set,
`visible` when it is not. The row goes only to the customer's own function,
and only the filled-in strings reach Apple or Google, which is the §2 posture.

Templated tables skip the per-scope cooldown, because a debounced banner is a
lost banner. The ceiling is one `pg_net` request per templated row, and the
operator opted into that by choosing the table.

The rows come from `nostos link --visible <entry>`, where `<entry>` uses the
`NOSTOS_PUSH_TABLES` visible/action grammar, so one line of config works in
both modes. The generated SQL replaces the whole set rather than merging into
it. `--deploy --fcm-service-account <json>` then rolls out the rest through
the `supabase` CLI: it applies the SQL with `pg_net`, mints the shared secret
on both sides, and deploys the function. `--push` writes the function into the
app repo, compiled into the binary so it matches the SQL it was generated
with. Per app, only the Firebase/APNs setup and the app's notification
categories are still manual.

### 3. Token registry in the customer's Postgres, tenant force-stamped

`cairn_push_tokens(token, platform, account_id, tenant_id, updated_at)` —
server-internal table like `cairn_oplog`, managed by a `PgTokenStore` following
the `PgWriteBack` pool-of-one pattern. Registration via REST (`POST
/push-tokens`), authenticated by the same JWT path as `/sync`; `tenant_id` /
`account_id` are stamped server-side from `Principal::tenant_scope` (ADR-0018
discipline — client-attested tenant on a token row is an
exfiltration-adjacent bug). Prune on APNs 410 / FCM `UNREGISTERED`. Every SDK
deregisters in its sign-out hook (ADR-0034 hook pattern) — a leaked
registration would push the previous principal's data to the next user.

### 4. Presence from the session store; coalescing in-rail

"Offline" = no live session for the account in `SessionStore` — never socket
liveness (eviction leaves zombie sockets) and never `Dropped` (a slow-online
client must not be double-signalled). The router enqueues `(table, tenant,
account, lsn)` hints into a bounded channel off the fan-out hot loop
(non-blocking, `OpLogWriter` contract — the 833k ops/sec path must not gain a
PG round-trip). A background coalescer debounces per account (digest window),
then sends with rail-native supersede semantics: FCM `collapse_key`, APNs
`apns-collapse-id`, Web Push `Topic` — keyed per (device, subscription).
Staleness is bounded by the rails: `apns-expiration: 0` for silent pings, short
FCM `ttl`, Web Push `TTL`. Priority defaults to the power-cheap tier
(`apns-priority: 5`, FCM `normal`, Web `Urgency: low`); a per-table map may
raise user-visible events to the wake tier.

### 5. Enhancements shipped in v1

- **Delivery observability:** `push_sent` / `push_failed` counters plus
  push-LSN → client-ack correlation. The sync engine's per-device LSN acks
  answer "did the device actually get the data" — structurally impossible for
  pure-push vendors; this is the headline.
- **Digest window:** the coalescer's per-account debounce (Novu-style
  look-back) collapses bursts to one push per account per window.
- **Per-table priority/template config** (env or rules file, consistent with
  `nostos_rules.toml`).
- **Live Activities (iOS):** ActivityKit push tokens registered like device
  tokens; state updates ride priority-5 (budget-free) sends; start/update/end
  mapped from sync events for tables the app declares live. Flagged
  experimental at first (token-rotation bookkeeping).
- **Web Push rail (server-side, core):** direct VAPID sends with
  `Topic`/`Urgency`/`TTL` headers — no FCM intermediary on the web rail.

### 6. SDK sequencing; web is v1-experimental

- **Wave 1:** server core + flutter (the only SDK with `resume()` today) +
  node (registration symmetry).
- **Wave 2:** UniFFI four (kotlin/swift/dotnet) + react_native — requires a
  **non-destructive teardown + wake API** (`disconnect`/`resume` siblings;
  today `sign_out` wipes local state, unusable for push wake). This gap exists
  independent of push; push is its first consumer.
- **Wave 3 (experimental):** web + capacitor. Requires architectural work, not
  API surface: the wasm engine's `Window::localStorage` dependency must be
  abstracted (SW context has no `Window`), a real Service Worker with a
  `push` handler must replace/augment the module Worker, and Capacitor needs a
  new native plugin (WKWebView has no web push). Ships behind a flag,
  ADR-0033 degrade-path discipline, until proven.

## Consequences

- **Positive:** first sync-aware push in the category; targeting, token
  hygiene and multi-device fan-out become the same code path as sync fan-out;
  delivery observability no push vendor can match; self-hosted Apache-2.0
  answer to proven demand (ntfy 33k stars) without ntfy's iOS workaround.
- **Positive:** zero risk to the throughput moat — push is strictly off the hot
  loop, additive REST + background task.
- **Negative:** nostos inherits rail churn (FCM `token`→`fid` deprecation
  already noted) and Apple/Google best-effort last-mile. Honest posture: push
  is a nudge; sync reconciles. Marketing must never promise push delivery
  guarantees.
- **Negative:** visible templates put notification content on the server —
  tenant isolation of templates is a config-surface responsibility (documented
  footgun, `NOSTOS_WRITE_TABLES`-style).
- **Closed hole:** the streaming path's column extractor was string-only, so
  predicates over numeric/bool columns matched wider than intended; typed
  extraction landed with plan task 1.4
  (`extract_typed_column`, delegates to the canonical `extract_json_column`
  mapping per ADR-0019).

## The test that matters

Two devices share an account; device A offline. A committed change matching
the account's predicate ⇒ exactly one coalesced push (burst of 100 events ⇒ 1
push) ⇒ A's OS shows the templated notification with the app killed ⇒ opening
the app resumes from the durable checkpoint and applies the data (assert LSN
caught up). Separately: sign-out deregisters the token (next principal
receives nothing); an APNs 410 prunes the row; the hot loop's latency is
unchanged with push enabled (bench gate).
