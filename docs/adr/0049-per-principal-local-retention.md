---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "Medium. The principal is persisted in every device's `nostos_meta` (SQLite) and read by `DirectClient` before each sync; the flag is a public parameter on `NostosDatabase.direct`, `Nostos.direct`, `DirectNostosEngine.connect` and the frb `connect`. Removing it breaks every app that passed it, and a store already carrying a principal has to be migrated or wiped."
  surprising_without_context: true
  surprise_reason: "ADR-0029 says sign-out wipes, full stop, and its tests pin that. A reader finds a mode where sign-out keeps every row on the device and the wipe happens later, inside `sync()`, keyed on an UNVERIFIED JWT claim. Without this record that reads as a security regression rather than a deliberate, opt-in, per-principal cache."
  result_of_real_tradeoff: true
  rejected_alternatives: "(1) Flip the default to keep: PowerSync, Replicache and Zero all default to per-user segregation or wipe-on-logout; a shared-device app that upgraded would silently retain the previous user's rows. (2) App-level: call `close()` instead of `signOut()` and reopen — works for one app, but every app re-implements the principal check and gets it wrong once. (3) One SQLite file per user id (Replicache's `name`, Zero's `userID`): correct but multiplies disk use on shared devices and leaves orphan files nobody deletes; the single-store + principal check gives the same guarantee for the common one-user-per-device case. (4) Verify the JWT locally before trusting `sub`: needs the project's JWKS on the device for a check the server already performs on every request; a forged `sub` only decides WHICH local rows are shown, and RLS still decides which rows are pulled."
  all_three_true: true
status: accepted
---

# ADR-0049: Per-principal local retention — an opt-in that keeps rows across sign-out

- **Status:** Accepted (2026-09-25).
- **Date:** 2026-09-25
- **Amends:** ADR-0029 (sign-out wipes) — that stays the default.
- **References:** ADR-0029, `crates/nostos-client/src/direct.rs` (`LocalRetention`),
  `crates/nostos-core/src/storage.rs` (`principal`/`save_principal`),
  `crates/nostos-client/tests/direct_mode_client.rs` (the three retention tests).

## Context

An offline-first app is expected to download its data once per install. Under
ADR-0029 every `signOut` wipes the device, so a user who signs out and back in
on the same phone re-snapshots everything (measured on atlet iOS 2026-09-25:
1000 products + sessions, ~300 KB, on every sign-in). The wipe is right for a
shared device and wrong for the far more common one-person phone.

What the field does (docs read 2026-09-25):

- **PowerSync** `disconnectAndClear({clearLocal: true})` — "Disconnect and
  clear the database. Use this when logging out." `disconnect()` alone keeps
  the rows. Wipe is the documented logout path; keep is the caller's choice.
- **Replicache** `name` — "It is important to use user-specific names" so each
  user's local store is separate and a sign-in resumes.
- **Zero** `userID` — "Zero uses the userID field to segregate the client-side
  storage for each user. This allows users to quickly switch between multiple
  users and accounts without resyncing."

The norm: local storage is keyed by principal; wipe-on-logout is the safe
default and retention is opt-in.

## Decision

1. **`LocalRetention` on `DirectClient`**, default `WipeOnSignOut` (ADR-0029
   unchanged). `KeepForPrincipal` is opt-in per app.
2. **The principal is the JWT `sub`**, decoded (not verified) from the token in
   `set_token` and persisted in storage meta (`nostos_meta.principal`) by the
   first sync that runs under it. `Storage::clear` drops it with the rows.
3. **Under `KeepForPrincipal`, `sign_out` only drops the token.** The wipe is
   deferred to the start of the next `sync()`, before the push, under the
   source lock: same `sub` as the stored principal → resume the log; a
   different `sub`, or no stored principal on a store that already has a
   horizon → full wipe (rows, outbox, epoch, horizon, principal), fresh cursor,
   bootstrap from the snapshot, then persist the new `sub`.
4. **Defaults degrade to one extra snapshot, never a leak.** A store written
   before this ADR has no principal and is treated as someone else's. A
   `Storage` that leaves `principal()` at the trait default (`None`) wipes on
   every foreign-or-unknown sign-in. A token with no `sub` never adopts and
   never retains.
5. **Flag surface:** `keepLocalOnSignOut: bool` (default `false`) on
   `NostosDatabase.direct`, `Nostos.direct`, `DirectNostosEngine.connect`,
   `createDirectNostosEngine`, and the frb `NostosDirectHandle.connect`. atlet
   reads it from `--dart-define ATLET_KEEP_LOCAL=true`.

Why `sub` unverified is enough: the server verifies the signature on every
pull and RLS decides which rows come down. A forged `sub` on the device can at
worst make the client show rows it already held for another user — but only if
the attacker already holds the device unlocked with that other user's rows on
it, which is the threat the DEFAULT handles by wiping.

## Alongside: gzip on the pull client

The same measurement showed the snapshot travelling uncompressed. reqwest's
`gzip` feature is now on for the workspace ("Enable auto gzip decompression by
checking the `Content-Encoding` response header … the `Accept-Encoding` header
is set to `gzip`" — reqwest 0.12 `ClientBuilder::gzip` docs). Supabase answers
`content-encoding: gzip` when asked.

| `POST /rest/v1/rpc/nostos_snapshot`, atlet project, 2026-09-25 | bytes | wall (3 runs) |
|---|---|---|
| before (`Accept-Encoding: identity`) | 301,456 | 1.40 s / 0.41 s / 0.41 s |
| after (`Accept-Encoding: gzip`) | ~40,800 | 1.03 s / 0.70 s / 0.40 s |

7.4× fewer bytes. Wall time on a warm Wi-Fi link is dominated by the RPC, not
the transfer, so the latency win shows on cold connections and cellular, not
in this table. Measured with curl from the same machine; reqwest sends the
same header.

## Consequences

- Same user, same phone: one snapshot per install. Sign-out costs nothing on
  the next sign-in.
- Different user: identical to ADR-0029, just one sync later. Between the
  sign-out and that sync the rows are on disk under no token; the app decides
  whether that is acceptable (it is the same exposure as `close()`).
- **ponytail: direct mode only.** Server mode (`SyncClient::clear_local_state`,
  the frb `nostos.rs` `sign_out`) still wipes unconditionally; add the same
  `LocalRetention` there when a server-mode app asks.
- **ponytail: the Flutter `signOut` hooks still run.** `NostosDatabase.signOut`
  awaits the registered hooks (T6 blob-store wipe) regardless of the flag, so
  attachments are wiped even in keep mode. Gate the hooks on the flag when an
  app with attachments opts in.
- The `principal` row is one more key in `nostos_meta`; no schema migration.

## The test that matters

`direct_mode_client.rs`: default wipes and re-snapshots; keep mode retains
rows + horizon + principal and the next sync of the same `sub` does not
snapshot; a different `sub` wipes (including a queued write from the previous
user) and bootstraps afresh.
