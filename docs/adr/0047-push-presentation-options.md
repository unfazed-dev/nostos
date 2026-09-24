---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "High once used. The `[k=v,…]` group is operator config (NOSTOS_PUSH_TABLES, `nostos link --visible`), a pushd API field (`visible.options`), a stored column (`nostos.push_templates.options`), and the `nostos_*` payload keys an app's Notification Service Extension reads. Renaming a key breaks deployed configs and shipped app extensions at once."
  surprising_without_context: true
  surprise_reason: "The keys are nostos's own vocabulary (`collapse`, `level`, `image`), not APNs/FCM field names, and some of them do nothing without app-side code (an NSE for image/sender, capabilities for time-sensitive and Communication Notifications). A reader expecting a vendor passthrough would find neither `interruption-level` nor `notification_priority` in the config."
  result_of_real_tradeoff: true
  rejected_alternatives: "Raw vendor passthrough (an `apns` and an `fcm` JSON block per table: JSON-in-env, two spellings of one intent, and the Edge Function would need a third); one env var per option (does not scale per table); a JSON template per table (the liveactivity precedent, but title/body/route already live in the colon grammar and a second syntax per entry is worse than one bracket group)."
  all_three_true: true
status: accepted
---

# ADR-0047: Push presentation options — one vocabulary, every rail

- **Status:** Accepted (2026-09-25).
- **Date:** 2026-09-25
- **References:** ADR-0037 (sync-aware push, the `visible`/`action` modes and
  routing keys), ADR-0038 (the push daemon), `docs/api/push.md` (the key
  table), Apple "Generating a remote notification" / "Modifying content in
  newly delivered notifications" / "Implementing communication
  notifications", FCM HTTP v1 `Message` reference.

## Context

A visible push rendered title, body, category and a tap route — nothing else.
The rest of what a notification can do (an image, a subtitle, replacing the
previous one instead of stacking, grouping, interruption level, a sender with
a picture the way messaging apps show one) is spelled differently on every
vendor, sometimes in a header, and sometimes needs app-side code. Each app
would otherwise rediscover the mapping per rail, per mode.

## Decision

1. **One `[k=v,…]` group**, right after the mode and route, in every entry
   point: `NOSTOS_PUSH_TABLES`, `nostos link --visible`, pushd's
   `visible.options`. One parser (`nostos_infra::push::take_options`) and one
   validator (`validate_options`). Keys: `subtitle`, `image`, `thread`,
   `collapse`, `level`, `relevance`, `sound`, `channel`, `sender`, `avatar`.
   Values take `{col}`; `level`/`relevance`/`sound`/`channel` must be literal
   so a typo fails at startup.
2. **nostos maps, the operator does not.** Each rail translates the keys to
   its vendor's fields (`OPTION_KEYS` doc table; the direct-mode Edge
   Function mirrors `fcm.rs`). A `collapse` option outranks the router's
   per-table collapse key.
3. **What the OS cannot render alone goes through an NSE.** `image`, `sender`
   and `avatar` set `mutable-content` and ride as `nostos_*` keys (reserved
   from `data`). The Swift SDK ships `NostosNotificationService`, a
   dependency-free SPM product: an app's extension is one subclass.
   `sender`/`avatar` become a Communication Notification only with the app's
   capability — Apple's rule, documented, not worked around.
4. **APNs direct plays the default sound** on visible pushes, as the FCM
   rail always did; `sound=none` opts out.

## Consequences

- An older nostos-pushd rejects `visible.options` (`deny_unknown_fields`);
  the delegating client omits the field when empty, so only a push that uses
  options needs the newer daemon.
- A project linked before this gains the `options` column in place (`alter
  table … add column if not exists`) on the next `nostos link`.
- Android sender rendering (MessagingStyle) is the app's for now: the keys
  arrive in `data`. ponytail: the Kotlin/Flutter SDKs grow a renderer when an
  app asks for it.
- Skipped until asked: `badge` (a per-user count is not a per-table
  template), accent colour and small icon (app resources, not payload).
