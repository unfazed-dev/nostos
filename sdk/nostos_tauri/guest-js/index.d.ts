/**
 * @nostos-sync/tauri — typed guest bindings for the nostos Tauri 2 plugin.
 *
 * Mirrors the Rust surface in sdk/nostos_tauri/src/lib.rs one-for-one, plus
 * the unified-verb sugar tier (upsert/patch/deleteRow/writeBatch/watchRows/
 * fetchAll). Requires @tauri-apps/api ^2 as a peer dependency.
 */

import type { Channel } from "@tauri-apps/api/core";

/**
 * One reactive-watch snapshot pushed from Rust (a full table snapshot per
 * change tick — self-healing on lag, not a diff). `rows` has the same shape
 * query() rows have.
 */
export interface NostosSnapshot {
  table: string;
  rows: Array<{
    pk: string;
    payload: Record<string, unknown>;
  } & Record<string, unknown>>;
}

/** Options for {@link NostosRaw.connect}: all optional when the plugins.cairn
 *  config block in tauri.conf.json supplies defaults. */
export interface ConnectOptions {
  /** Sync endpoint, e.g. "ws://127.0.0.1:8080/sync". Falls back to
   *  plugins.cairn.syncUrl. */
  url?: string;
  /** Bearer JWT for the sync connection + push-token REST. Falls back to
   *  plugins.cairn.token. */
  token?: string | null;
  /** On-device SQLite path (absolute recommended). Falls back to
   *  plugins.cairn.dbPath, then "cairn.db". */
  dbPath?: string;
}

/** The outbox status (ADR-0027) — the `deadLetters()` result. */
export interface NostosWriteStatus {
  /** Writes durably queued, not yet server-ack'd. > 0 offline is normal. */
  pending: number;
  /** Writes permanently failed this session (quarantined, inspectable). */
  deadLettered: number;
  /** The server's most recent dead-letter error, verbatim. */
  lastError: string | null;
}

/** A write enqueued through the sugar tier's writeBatch. */
export interface BatchWrite {
  table: string;
  op?: "upsert" | "delete" | "patch";
  pk: string;
  payload?: Record<string, unknown> | null;
}

/**
 * Raw tier — the exact Rust command surface. See index.js docs for the
 * per-command semantics (mirroring the Rust doc comments).
 */
export interface NostosRaw {
  /** Open the local store + build the SyncClient. No network I/O — follow
   *  with subscribe() or watch() or nothing replicates. */
  connect(options?: ConnectOptions): Promise<void>;
  /** Start the live-replication run loop. */
  subscribe(table: string): Promise<void>;
  /** Enqueue a durable local write; resolves with the outbox id. */
  write(
    table: string,
    op: "upsert" | "delete" | "patch",
    pk: string,
    payloadJson: string | null,
  ): Promise<number>;
  /** Run a SELECT; resolves with a JSON array-of-objects string. */
  query(sql: string): Promise<string>;
  /** Read the durable LSN checkpoint (fresh store = 0). */
  checkpoint(): Promise<number>;
  /** Reactive full-snapshot push per change tick. Returns a disposer. */
  watch(table: string, onSnapshot: (snapshot: NostosSnapshot) => void): () => void;
  /** Swap the auth token on the live client. */
  setToken(token: string | null): Promise<void>;
  /** Wipe local state + deregister push tokens + drop the session. */
  signOut(): Promise<void>;
  /** POST /push-tokens with the sync credential. */
  registerPushToken(platform: "fcm" | "apns" | "webpush", token: string): Promise<void>;
  /** DELETE /push-tokens/{token}. */
  deregisterPushToken(token: string): Promise<void>;
  /** ADR-0030 OR-set add (add-wins). Table must be in orSetTables config. */
  orSetAdd(table: string, pk: string, element: string): Promise<number>;
  /** ADR-0030 OR-set remove (tombstone; add-wins on re-add). */
  orSetRemove(table: string, pk: string, element: string): Promise<number>;
  /** ADR-0030 PN-Counter increment. Table must be in counterTables config. */
  counterIncrement(table: string, pk: string, delta: number): Promise<number>;
  /** ADR-0030 PN-Counter decrement. */
  counterDecrement(table: string, pk: string, delta: number): Promise<number>;
  /** ADR-0027 outbox status (pending/deadLettered/lastError). */
  deadLetters(): Promise<NostosWriteStatus>;
  /** True once the session has proven a subscription. */
  connectionState(): Promise<boolean>;
}

export declare const nostos: NostosRaw;

/** Sugar: upsert one row (object payload). Resolves with the outbox id. */
export declare function upsert(
  table: string,
  pk: string,
  payload: Record<string, unknown>,
): Promise<number>;

/** Sugar: column-level update — send only the changed columns. */
export declare function patch(
  table: string,
  pk: string,
  changedColumns: Record<string, unknown>,
): Promise<number>;

/** Sugar: delete one row by primary key. */
export declare function deleteRow(table: string, pk: string): Promise<number>;

/** Sugar: enqueue N writes; resolves with outbox ids in order. */
export declare function writeBatch(writes: BatchWrite[]): Promise<number[]>;

/** Sugar: watch + hand each snapshot's parsed rows to onRows. */
export declare function watchRows(
  table: string,
  onRows: NostosSnapshot["rows"],
): () => void;

/** Sugar: query + JSON.parse in one step. */
export declare function fetchAll(sql: string): Promise<NostosSnapshot["rows"]>;

export default nostos;
