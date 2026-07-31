// Type declarations for @nostos-sync/web — reduced-scope feasibility proof.
// See index.js header for the ceiling / upgrade-path notes.

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
}
