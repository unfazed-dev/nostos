/**
 * @nostos-sync/tauri — typed guest bindings for the nostos Tauri 2 plugin.
 *
 * Thin wrappers over invoke("plugin:cairn|<command>") mirroring the Rust
 * surface in sdk/nostos_tauri/src/lib.rs. Two tiers, one import:
 *
 *   import { nostos, upsert, query, watch } from "@nostos-sync/tauri";
 *   await nostos.connect({ url, token, dbPath });
 *   const id = await upsert("tasks", "t1", { title: "Walk dog" });
 *
 * - Raw tier: connect/subscribe/write/query/checkpoint — the exact command
 *   surface (write takes op + a pre-stringified payload; query returns a JSON
 *   string). Kept verbatim so a JS caller can always drop to the metal.
 * - Sugar tier: upsert/patch/delete/writeBatch — the unified-verb naming the
 *   nostos-dx-audit standardizes on (payloads pass objects, not strings).
 *
 * Command args are camelCase on the wire (Tauri's default
 * ArgumentCase::Camel); these wrappers spell them the same way.
 *
 * @license Apache-2.0
 */

import { invoke, Channel } from "@tauri-apps/api/core";

/** The invoke prefix every nostos command is namespaced under. */
const CMD = "plugin:cairn|";

/**
 * Raw tier — the exact Rust command surface.
 *
 * connect() does NO network I/O: it opens SQLite + builds the client. A
 * subscribe() (or watch()) must follow or no server-pushed row ever arrives.
 * Every field is optional when the plugins.cairn config block in
 * tauri.conf.json supplies defaults (syncUrl / token / table / dbPath).
 */
export const nostos = {
  /** Open the local store + build the SyncClient. No network I/O. */
  connect(options = {}) {
    const { url, token, dbPath } = options ?? {};
    return invoke(`${CMD}connect`, { url, token, dbPath });
  },

  /** Start the live-replication run loop (server -> on-device rows). */
  subscribe(table) {
    return invoke(`${CMD}subscribe`, { table });
  },

  /**
   * Enqueue a durable write. Resolves with the outbox id once the write is
   * durable LOCALLY (ADR-0013), not when the server acks it. op is
   * "upsert" | "delete" | "patch"; payloadJson is a JSON string or null.
   */
  write(table, op, pk, payloadJson) {
    return invoke(`${CMD}write`, { table, op, pk, payloadJson });
  },

  /** Run a SELECT; resolves with a JSON array-of-objects string. */
  query(sql) {
    return invoke(`${CMD}query`, { sql });
  },

  /** Read the durable LSN checkpoint (u64; fresh store = 0). */
  checkpoint() {
    return invoke(`${CMD}checkpoint`);
  },

  /**
   * Reactive watch (ADR-0024): push the full table snapshot to onSnapshot
   * immediately, and again after every change tick (remote apply OR local
   * write) — a Rust->JS push, NOT a poll. Drop the returned disposer (or
   * call it) to end the pump; signOut tears everything down too.
   */
  watch(table, onSnapshot) {
    const channel = new Channel();
    channel.onmessage = onSnapshot;
    const done = invoke(`${CMD}watch`, { table, onEvent: channel }).then(() => () => {});
    // The pump self-terminates when JS drops the Channel; this disposer is
    // belt-and-braces for explicit teardown.
    return () => {
      channel.handler = undefined; // drop the JS-side handler
    };
  },

  /** Swap the auth token on the LIVE client (ADR-0029 refresh self-heal). */
  setToken(token) {
    return invoke(`${CMD}set_token`, { token });
  },

  /**
   * ADR-0029 sign-out: stop sync, wipe local rows + outbox + checkpoint,
   * clear the token, deregister session push tokens, drop the session.
   * Idempotent.
   */
  signOut() {
    return invoke(`${CMD}sign_out`);
  },

  /**
   * ADR-0037 §3: register this device's push token (POST /push-tokens) with
   * the same auth the sync connection uses. platform is "fcm" | "apns" |
   * "webpush"; on iOS/Android the token comes from the shell's native push
   * hooks. Desktop apps usually skip this (no OS rail — WS delivers).
   * Registered tokens are deregistered by signOut automatically.
   */
  registerPushToken(platform, token) {
    return invoke(`${CMD}register_push_token`, { platform, token });
  },

  /**
   * ADR-0037 §3: deregister one push token (DELETE /push-tokens/{token});
   * call when the app can no longer receive on it.
   */
  deregisterPushToken(token) {
    return invoke(`${CMD}deregister_push_token`, { token });
  },

  // ---- unified verbs: CRDT tier (ADR-0030) + observability ----

  /**
   * ADR-0030 add-wins OR-set: add `element` to the OR-set at (table, pk).
   * The table must be declared in plugins.cairn.orSetTables AND match the
   * server's NOSTOS_OR_SET_COLUMNS (three views of one truth). Resolves
   * with the outbox id once the merge-upsert is durable locally.
   */
  orSetAdd(table, pk, element) {
    return invoke(`${CMD}or_set_add`, { table, pk, element });
  },

  /**
   * ADR-0030 OR-set remove — a tombstone at a fresh HLC; a concurrent or
   * later re-add (higher HLC) re-activates the element (add-wins).
   */
  orSetRemove(table, pk, element) {
    return invoke(`${CMD}or_set_remove`, { table, pk, element });
  },

  /** ADR-0030 PN-Counter increment by delta (this replica's positive side). */
  counterIncrement(table, pk, delta) {
    return invoke(`${CMD}counter_increment`, { table, pk, delta });
  },

  /** ADR-0030 PN-Counter decrement by delta (this replica's negative side). */
  counterDecrement(table, pk, delta) {
    return invoke(`${CMD}counter_decrement`, { table, pk, delta });
  },

  /**
   * ADR-0027 outbox status: { pending, deadLettered, lastError }. pending
   * > 0 offline is the offline-first promise WORKING; deadLettered > 0
   * means writes permanently failed (inspect + surface lastError — it
   * names the exact env var for allowlist rejections).
   */
  deadLetters() {
    return invoke(`${CMD}dead_letters`);
  },

  /**
   * True once the session has PROVEN a subscription (first frame or write
   * ack landed). false = connected-not-yet-proven or fully offline.
   */
  connectionState() {
    return invoke(`${CMD}connection_state`);
  },
};

// ---------------------------------------------------------------------------
// Sugar tier — the unified-verb naming standardized in the nostos-dx-audit
// (docs/plans/nostos-integration-tauri-flutter-push.md, DX section). Same
// commands under the hood; objects instead of pre-stringified payloads.
// ---------------------------------------------------------------------------

/**
 * Upsert one row (object payload). The write applies locally at once and
 * flushes to the server on the next connect — the offline-first contract.
 * Resolves with the outbox id.
 */
export function upsert(table, pk, payload) {
  return nostos.write(table, "upsert", pk, JSON.stringify(payload ?? {}));
}

/** Column-level update: send only the changed columns. */
export function patch(table, pk, changedColumns) {
  return nostos.write(table, "patch", pk, JSON.stringify(changedColumns ?? {}));
}

/** Delete one row by primary key. */
export function deleteRow(table, pk) {
  return nostos.write(table, "delete", pk, null);
}

/**
 * Fire N writes through the raw tier without awaiting each — they enqueue
 * independently into the same outbox, so one await-per-write is unnecessary.
 * Resolves with the outbox ids in order. (The Rust writeBatch verb lands
 * with the unified-verb wave; this composes the same contract from JS.)
 */
export async function writeBatch(writes) {
  const ids = [];
  for (const w of writes) {
    ids.push(
      await nostos.write(
        w.table,
        w.op ?? "upsert",
        w.pk,
        w.payload == null ? null : JSON.stringify(w.payload),
      ),
    );
  }
  return ids;
}

/**
 * watch + parse in one step: subscribe to the reactive snapshot stream and
 * hand each parsed row array to onRows. Returns a disposer.
 */
export function watchRows(table, onRows) {
  return nostos.watch(table, (snapshot) => onRows(snapshot.rows));
}

/**
 * query + JSON.parse in one step. Prefer this over raw query() unless the
 * raw string is wanted.
 */
export async function fetchAll(sql) {
  return JSON.parse(await nostos.query(sql));
}

export default nostos;
