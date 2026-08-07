// Type declarations for @nostos-sync/web — browser (live WS via Worker) + node smoke.
//
// Two entry shapes:
// - Browser: index.js is NOT used directly; the Worker (worker/nostos.worker.js)
//   loads the `--target web` wasm artifact and exposes a postMessage protocol.
//   The app host (e.g. e2e/app.html) is a thin proxy. See docs/api/web.md.
// - Node: NostosClient (below) drives the apply engine only (no live transport).
//   The ceiling is documented in index.js's header.
//
// ADR-0033: in the browser Worker, the storage backend is either "durable"
// (OPFS-backed SQLite-WASM via opfs-sahpool) or "memory" (InMemoryStorage
// fallback when OPFS is unavailable). The mode is surfaced on SyncStatus as
// `storageMode`. The node NostosClient is always "memory" (no OPFS in Node).

export interface NostosClientConfig {
  url?: string | null;
  token?: string | null;
  table?: string | null;
}

export interface WriteResult {
  /** Durable LSN after the write (resume_lsn on reconnect). */
  checkpoint: number;
  /** Rows committed by this write. */
  rowsApplied: number;
}

export interface Row {
  pk: string;
  payload: Buffer;
}

/**
 * The storage backend mode (ADR-0033).
 * - "durable" — OPFS-backed SQLite-WASM (opfs-sahpool). Rows + outbox +
 *   checkpoint survive a page reload. Browser Worker only.
 * - "memory" — InMemoryStorage. Nothing survives a reload. This is the node
 *   ceiling AND the browser degrade path (Safari Private Browsing, old
 *   browsers, OPFS disallowed).
 */
export type StorageMode = "durable" | "memory";

/**
 * Sync status surfaced to the UI. In the browser, the Worker pushes
 * {type:"storage", mode} after init and {type:"status", connected} on connect.
 */
export interface SyncStatus {
  connected: boolean;
  storageMode: StorageMode;
}

/**
 * PowerSync-style sync client. Reduced-scope: no live WS transport in
 * node — drives the apply engine only.
 */
export declare class NostosClient {
  constructor(config?: NostosClientConfig);
  connect(): Promise<NostosClient>;
  subscribe(table: string, whereSql?: string | null): NostosClient;
  write(table: string, pk: string | number, payload: Uint8Array | number[]): WriteResult;
  query(table: string): Row[];
  watch(table: string, callback: (rows: Row[]) => void): () => void;
  /** ADR-0029: wipe rows + outbox + cached token. */
  signOut(): void;
  /** ADR-0029 §3: cache a new JWT for the next connect. */
  setToken(newToken: string | null): void;
  readonly checkpoint: number;
  readonly rowCount: number;
  /** Storage backend mode — always "memory" in the node smoke (no OPFS). */
  readonly storageMode: StorageMode;
}
