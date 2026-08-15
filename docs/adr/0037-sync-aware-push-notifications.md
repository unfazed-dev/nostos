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
partial-replication engine ships this (research §1): PowerSync, ElectricSQL,
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
