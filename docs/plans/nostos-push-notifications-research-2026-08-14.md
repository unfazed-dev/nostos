# Push notifications — research synthesis (2026-08-14)

Grounds the decision to build a sync-aware push router (ADR pending). Six parallel
research passes: 4 web (engine parity, FCM pain points, rail features, prior art),
2 codebase (server integration map, per-SDK feasibility). Key citations inline;
full agent logs in the session archive.

## 1. The territory is unoccupied

No Postgres→SQLite partial-replication engine ships built-in device push:
ElectricSQL, Replicache/Zero, Ditto (sitemap-level
negative — no public push docs at all), RxDB, WatermelonDB, InstantDB, Supabase
(guides to DIY Edge Function + FCM). Nearest neighbor: Convex's first-party
Expo-push component — user-targeted visible notifications, not sync-aware wake.
BaaS (Firestore/FCM, CloudKit) integrate push natively but own your database —
the gap is specific to partial replication over your own DB.

## 2. What incumbents structurally lack (exploitable pain)

- **No data-aware targeting** — FCM topics are public broadcast strings; every
  push tool needs hand-wired DB triggers/segments (FCM topic docs 🔥, Supabase
  guide 🔥). A router evaluating predicates over the replication stream is the
  one architecture where "notify on this row for users matching X" is native.
- **FCM topic fan-out is throughput-not-latency, capacity shared across
  projects** (1k concurrent fanouts/project 🔥); direct token sends bypass it.
- **Token lifecycle is everyone's chore** — Apple requires re-fetch on every
  launch 🔥; ~15% loss attributed to poor hygiene 🔥. A sync engine already owns
  a per-device session registry; prune-on-410/Unregistered is one more column.
- **Per-user multi-device has no first-class primitive** (FCM device groups are
  legacy, ≤20 tokens 🔥). User-as-principal with N device sessions is the sync
  engine's native model.
- **Self-host demand is proven**: ntfy = 33.5k stars, but iOS self-host delivery
  requires polling upstream ntfy.sh (Apple background restrictions). A
  self-hosted Apache-2.0 router speaking APNs directly avoids that workaround.

## 3. Prior art — patterns and anti-patterns

Closest ancestor: **Parse Server's `where`-query push targeting** over the
`_Installation` table (server-side query at send time) — proves query targeting
works; its decoupling from data changes (manual `afterSave` wiring) is the exact
anti-pattern. Ably/PubNub push carries channel content (a second data path).
Novu's digest engine is the coalescing reference. FCM's own docs frame data-only
push as "new data available to sync" with `onDeletedMessages()` → "perform a
full sync" — doorbell semantics are Google's own prescription.

**Steal:** (1) push as content-free wake signal — payload is at most
`{table, lsn}`; (2) device registry as session-store extension, token lifecycle
first-class; (3) collapse by subscription id + short server-side debounce.

**Avoid:** (1) content-carrying push (dual delivery path, bypasses predicates/
ACLs, leaks row data through vendors, double-delivers vs the WS); (2) a second
push-targeting registry parallel to sync subscriptions (they drift — offline
devices see a different data view than online ones).

## 4. Rail leverage (verified against FCM REST ref, Apple docs, RFC 8030/8292)

- **In-rail coalescing on all three rails**: FCM `collapse_key` (≤4 live),
  APNs `apns-collapse-id` (≤64B), Web Push `Topic` header (RFC 8030 §5.4, the
  most underused header in dashboards). Keyed per (device, subscription) =
  newest-wins "sync past lsn N" supersede; the router sheds its dedupe queue
  into the rails.
- **Expiry as staleness bound**: `apns-expiration: 0` (never store silent
  pings), short FCM `ttl`, Web Push `TTL` (required by RFC). A ping older than
  a minute is worthless — make the rails discard it.
- **Priority tiers mapped to sync semantics**: Android `normal|high`, APNs
  `10|5|1` (`background` push-type mandates 5; iOS data-only caps at 5), Web
  `Urgency`. Power-cheap default; wake-the-device reserved for user-visible.
- **FCM batch send (500/req)** for connection-amortized fan-out; per-platform
  override blocks express rail semantics from one decision point.
- **iOS silent push is opportunistic by design**: newest-wins holding,
  force-quit discard, daily power budget 🔥 — push must be a hint; the LSN
  cursor pull is the correctness mechanism (matches nostos's model exactly).
- **2026-era**: FCM `token` deprecated in favor of `fid`; Push API draft adds
  declarative push (SW-eviction-proof); Live Activities update at priority 5
  are budget-free — the only rail surface that *renders synced state*.

## 5. Codebase integration map (verified at HEAD)

- **Relevance is already computed**: `fan_out` builds the matched-session set at
  `crates/nostos-application/src/fanout.rs:196-203` (table-index candidates +
  `Predicate::matches`) — the natural push hook.
- **Shape**: `PushNotifier` port (application) + APNs/FCM/WebPush adapters
  (infra) — ADR-0007 already names `PushSink` as the extension point. Token
  registry in customer Postgres (`cairn_push_tokens(token, platform,
  account_id, tenant_id, updated_at)`), `PgTokenStore` following the
  `PgWriteBack` pool-of-one pattern; tenant force-stamped server-side from
  `Principal::tenant_scope` (ADR-0018 discipline — never client-attested).
  Registration via REST route (`POST /push-tokens`) — touches zero SDK wire
  protocols. Delivery loop = background task copying the `OpLogWriter`
  bounded-buffer/non-blocking contract.
- **Landmines**: (1) `InMemorySessionStore` discards `Principal` — must be
  threaded through `StoredSession`/`SessionCandidate` for account-keyed push;
  (2) hot loop is sacred (~1.2µs/event at the 833k headline) — push work must
  be non-blocking enqueue; (3) presence derives from store membership, never
  socket liveness (eviction leaves zombie sockets); (4) `Dropped` ≠ offline;
  (5) the streaming path's column extractor is string-only (`main.rs:478-485`)
  — predicates over numeric columns match wider than intended today; a push
  router inheriting the matched set inherits this hole.
- **STRATEGY.md:214 already fixes semantics**: push is a wake-up trigger, not a
  data channel — keeps it out of the exactly-once/LSN machinery.

## 6. Per-SDK feasibility (at HEAD)

| SDK | wake-and-sync entry | blocker | effort |
|---|---|---|---|
| flutter | ✅ `resume()` exists | bg-isolate FRB handle sharing | S |
| node | ✅ `close()` + reconnect | none (no OS push) | S |
| tauri | ❌ no reconnect loop at all | desktop has no silent push | S/M |
| kotlin/swift/dotnet | ❌ none — `sign_out` wipes local state; need non-destructive teardown + poke API | none hard (FCM/APNs direct) | M each |
| react_native | ❌ inherits kotlin/swift gap | two native modules | M |
| web | ⚠️ reconnect pattern unexposed | module Worker ≠ Service Worker; wasm `Window::localStorage` dep breaks SW context; VAPID permission UX | L |
| capacitor | ❌ | webview has no web push; needs new native plugin | L |

Sign-out deregistration mandatory on every SDK (ADR-0034 hook pattern) — a
leaked registration sends the next principal's data as a visible notification.

## 7. Honest limits (do not paper over)

- Last-mile through APNs/FCM stays best-effort; no router lifts Apple's
  silent-push budget or vendor API churn (FCM legacy shutdown precedent).
- iOS-killed apps receive *visible* payloads reliably, silent wakes only
  opportunistically — hence v1 includes server-side visible templates.
- Web/Capacitor push is architecturally blocked until the SW/localStorage work
  lands; don't promise web parity at v1.
