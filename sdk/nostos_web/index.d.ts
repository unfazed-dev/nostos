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

// ─────────────────── T6 attachments (ADR-0034) ───────────────────

/** Lifecycle wire strings (mirror nostos_core::AttachmentState). */
export interface AttachmentConstants {
  TABLE: string;
  COL: {
    id: string;
    filename: string;
    size: string;
    mediaType: string;
    state: string;
    timestamp: string;
  };
  STATE: {
    queuedUpload: string;
    queuedDownload: string;
    queuedDelete: string;
    synced: string;
    archived: string;
  };
}

/** Remote blob storage — the developer's bucket. Idempotent under retry. */
export interface AttachmentStorageAdapter {
  upload(path: string, bytes: Uint8Array, mediaType: string): Promise<void>;
  download(path: string): Promise<Uint8Array>;
  delete(path: string): Promise<void>;
}

/** Local blob cache. wipe() is called on sign-out (ADR-0029). */
export interface BlobStore {
  put(id: string, bytes: Uint8Array): Promise<void>;
  get(id: string): Promise<Uint8Array | null>;
  remove(id: string): Promise<void>;
  wipe(): Promise<void>;
}

/** One attachment metadata row, decoded for the driver. */
export interface AttachmentRow {
  id: string;
  state: string;
  mediaType: string;
  filename: string;
}

/** Metadata-plane access (the synced `attachments` table). */
export interface AttachmentMetadataGateway {
  queuedRows(): Promise<AttachmentRow[]>;
  patchState(id: string, state: string): Promise<void>;
  upsertRow(row: Record<string, unknown>): Promise<void>;
  currentState(id: string): Promise<string>;
}

/** First-class Supabase Storage adapter (@supabase/supabase-js is a peer dep). */
export declare class SupabaseStorageAdapter implements AttachmentStorageAdapter {
  constructor(opts: {
    client?: import("@supabase/supabase-js").SupabaseClient;
    url?: string;
    key?: string;
    bucket: string;
    pathPrefix?: string;
  });
  upload(path: string, bytes: Uint8Array, mediaType: string): Promise<void>;
  download(path: string): Promise<Uint8Array>;
  delete(path: string): Promise<void>;
}

/** Browser OPFS blob cache. Throws in node (no navigator.storage). */
export declare class OpfsBlobStore implements BlobStore {
  constructor(dirName: string);
  put(id: string, bytes: Uint8Array): Promise<void>;
  get(id: string): Promise<Uint8Array | null>;
  remove(id: string): Promise<void>;
  wipe(): Promise<void>;
}

/** The attachment driver. Call pump() per tick (or wire to the conn signal). */
export declare class Attachments {
  constructor(opts: {
    gateway: AttachmentMetadataGateway;
    adapter: AttachmentStorageAdapter;
    blobStore: BlobStore;
    isOnline: () => Promise<boolean>;
    maxAttempts?: number;
    now?: () => Date;
  });
  lastErrorFor(id: string): string | null;
  queueUpload(opts: {
    filename: string;
    bytes: Uint8Array;
    mediaType: string;
    id?: string;
  }): Promise<string>;
  queueDownload(id: string): Promise<void>;
  remove(id: string): Promise<void>;
  pump(): Promise<void>;
}
