// @nostos-sync/web attachments — T6 two-plane blob sync (ADR-0034 / contract T6).
//
// Mirrors the Flutter driver (sdk/nostos_flutter/lib/src/attachments.dart) over
// the SAME pure state machine in nostos-core (crates/nostos-core/src/attachments.rs).
// The metadata plane is an ordinary synced `attachments` table; the blob plane
// is a developer-supplied AttachmentStorageAdapter. Blobs never transit the
// Nostos server (moat constraint).
//
// The driver depends on a small AttachmentMetadataGateway interface so it is
// testable in node WITHOUT the browser Worker / live transport: the gateway
// reads queued rows + patches state. In the browser, a Worker-backed gateway
// will wrap Wave-2's postMessage protocol (worker/nostos.worker.js) once the
// live write path lands; the node test here supplies an in-memory gateway.
//
// State wire strings are duplicated shallowly from nostos-core (ADR-0034
// §driver boundary) — five constants, guarded by the Rust crate's unit tests.

"use strict";

// @supabase/supabase-js is a PEER dependency: the driver + blob store + test
// load without it; only SupabaseStorageAdapter construction pulls it in (lazy
// require below). Apps that bring their own bucket impl never pay for it.

/** Table + column names the app MUST declare (and put in NOSTOS_WRITE_TABLES). */
const ATTACHMENTS_TABLE = "attachments";
const COL = {
  id: "id",
  filename: "filename",
  size: "size",
  mediaType: "media_type",
  state: "state",
  timestamp: "timestamp",
};

/** Lifecycle wire strings (mirror nostos_core::AttachmentState). */
const STATE = Object.freeze({
  queuedUpload: "queued_upload",
  queuedDownload: "queued_download",
  queuedDelete: "queued_delete",
  synced: "synced",
  archived: "archived",
});
const QUEUED = new Set([STATE.queuedUpload, STATE.queuedDownload, STATE.queuedDelete]);

/** Default cap on adapter retries before a queued row dead-letters to archived. */
const DEFAULT_MAX_ATTEMPTS = 5;

/**
 * on_success mapping mirror of nostos_core::AttachmentState::on_success.
 * upload/download → synced; delete → archived.
 */
function onSuccess(state) {
  switch (state) {
    case STATE.queuedUpload:
    case STATE.queuedDownload:
      return STATE.synced;
    case STATE.queuedDelete:
      return STATE.archived;
    default:
      return state;
  }
}

/**
 * Remote blob storage — the developer's bucket. Implementations talk to a
 * storage backend (Supabase Storage, S3, …); Nostos never sees the bytes.
 * Methods MUST be idempotent under retry (delete on a missing path is success).
 *
 * @typedef AttachmentStorageAdapter
 * @property {(path: string, bytes: Uint8Array, mediaType: string) => Promise<void>} upload
 * @property {(path: string) => Promise<Uint8Array>} download
 * @property {(path: string) => Promise<void>} delete
 */

/**
 * First-class AttachmentStorageAdapter for Supabase Storage.
 * Uses @supabase/supabase-js. `upsert: true` makes upload idempotent under
 * retry; a not-found on delete is swallowed so queued_delete converges.
 */
class SupabaseStorageAdapter {
  /**
   * @param {{client?: import("@supabase/supabase-js").SupabaseClient, url?: string, key?: string, bucket: string, pathPrefix?: string}} opts
   */
  constructor({ client, url, key, bucket, pathPrefix = "" }) {
    // Lazy require so the module loads without @supabase/supabase-js installed
    // (peer dep — apps with their own adapter never install it).
    this._client =
      client ?? require("@supabase/supabase-js").createClient(url, key);
    this._bucket = bucket;
    this._pathPrefix = pathPrefix;
  }

  _key(path) {
    return this._pathPrefix ? `${this._pathPrefix}${path}` : path;
  }

  async upload(path, bytes, mediaType) {
    await this._client.storage
      .from(this._bucket)
      .upload(this._key(path), bytes, {
        contentType: mediaType,
        upsert: true,
      });
  }

  async download(path) {
    const { data, error } = await this._client.storage
      .from(this._bucket)
      .download(this._key(path));
    if (error) throw error;
    return new Uint8Array(await data.arrayBuffer());
  }

  async delete(path) {
    // Idempotent: a missing object returns an error the app treats as converged.
    const { error } = await this._client.storage
      .from(this._bucket)
      .remove([this._key(path)]);
    if (error && !String(error.message || "").toLowerCase().includes("not found")) {
      throw error;
    }
  }
}

/**
 * Local blob cache (browser): one file per id under an OPFS directory. Builds
 * on Wave-2's OPFS durable storage (ADR-0033). Throws if constructed outside a
 * browser (no `navigator.storage`); node tests use an in-memory store instead.
 *
 * ponytail: ceiling — uses the async FileSystemAccessHandle API (Worker-safe).
 * A synchronous variant would need opfs-sahpool like the metadata store; the
 * blob cache is not on the apply hot path so async is fine. Upgrade path:
 * share the sahpool connection if a measurement shows contention.
 */
class OpfsBlobStore {
  /**
   * @param {string} dirName OPFS root-relative directory name (e.g. "nostos-blobs").
   */
  constructor(dirName) {
    if (typeof navigator === "undefined" || !navigator.storage) {
      throw new Error(
        "OpfsBlobStore requires a browser with OPFS (navigator.storage). " +
          "Use an in-memory BlobStore in node."
      );
    }
    this._dirName = dirName;
    this._rootPromise = navigator.storage.getDirectory().then((root) =>
      root.getDirectoryHandle(dirName, { create: true })
    );
  }

  async _file(id, create) {
    const dir = await this._rootPromise;
    return dir.getFileHandle(id, { create });
  }

  async put(id, bytes) {
    const handle = await this._file(id, true);
    const writable = await handle.createWritable();
    await writable.write(bytes);
    await writable.close();
  }

  async get(id) {
    try {
      const handle = await this._file(id, false);
      const file = await handle.getFile();
      return new Uint8Array(await file.arrayBuffer());
    } catch (_) {
      return null;
    }
  }

  async remove(id) {
    const dir = await this._rootPromise;
    await dir.removeEntry(id).catch(() => {
      /* idempotent */
    });
  }

  async wipe() {
    const root = await navigator.storage.getDirectory();
    await root.removeEntry(this._dirName, { recursive: true }).catch(() => {
      /* idempotent */
    });
    // Recreate so subsequent puts don't need to re-create the dir handle.
    this._rootPromise = root.getDirectoryHandle(this._dirName, { create: true });
  }
}

/**
 * @typedef AttachmentRow
 * @property {string} id
 * @property {string} state
 * @property {string} mediaType
 * @property {string} filename
 */

/**
 * @typedef AttachmentMetadataGateway
 * @property {() => Promise<AttachmentRow[]>} queuedRows Read rows in a queued state.
 * @property {(id: string, state: string) => Promise<void>} patchState Patch the state column.
 * @property {(row: Record<string, unknown>) => Promise<void>} upsertRow Upsert a metadata row.
 * @property {(id: string) => Promise<string>} currentState Read the current state of one row.
 */

/**
 * The attachment driver. Construct with a gateway (metadata-plane access), an
 * adapter (remote blob bucket), a blob store (local cache), and an online
 * predicate. Then call pump() per tick (or wire it to the connection signal).
 */
class Attachments {
  /**
   * @param {{gateway: AttachmentMetadataGateway, adapter: AttachmentStorageAdapter, blobStore: import("./attachments").BlobStore, isOnline: () => Promise<boolean>, maxAttempts?: number, now?: () => Date}} opts
   */
  constructor({ gateway, adapter, blobStore, isOnline, maxAttempts = DEFAULT_MAX_ATTEMPTS, now = () => new Date() }) {
    this._gateway = gateway;
    this._adapter = adapter;
    this._blob = blobStore;
    this._isOnline = isOnline;
    this._maxAttempts = maxAttempts;
    this._now = now;
    this._inFlight = new Set();
    this._attempts = new Map();
    this._nextEligible = new Map();
    this._lastErrors = new Map();
  }

  /** The last adapter error for an id (dead-letter reason). Local-only. */
  lastErrorFor(id) {
    return this._lastErrors.get(id) ?? null;
  }

  /**
   * Queue a blob for upload. Bytes cache locally; a metadata row is upserted
   * with state = queued_upload. Returns the attachment id (the storage key).
   * @param {{filename: string, bytes: Uint8Array, mediaType: string, id?: string}} opts
   * @returns {Promise<string>}
   */
  async queueUpload({ filename, bytes, mediaType, id }) {
    const attachmentId = id ?? this._newId();
    await this._blob.put(attachmentId, bytes);
    await this._gateway.upsertRow({
      [COL.id]: attachmentId,
      [COL.filename]: filename,
      [COL.size]: bytes.length,
      [COL.mediaType]: mediaType,
      [COL.state]: STATE.queuedUpload,
      [COL.timestamp]: this._now().getTime(),
    });
    return attachmentId;
  }

  /** Queue a download for an attachment whose metadata row already exists. */
  async queueDownload(id) {
    await this._gateway.patchState(id, STATE.queuedDownload);
  }

  /** Queue a blob for deletion from the remote bucket (metadata retained). */
  async remove(id) {
    await this._gateway.patchState(id, STATE.queuedDelete);
  }

  /** One driver tick. Reads queued rows (when online) + dispatches blob ops. */
  async pump() {
    if (!(await this._isOnline())) return;
    let rows;
    try {
      rows = await this._gateway.queuedRows();
    } catch (_) {
      return; // gateway not ready; next tick retries.
    }
    const now = this._now();
    for (const row of rows) {
      if (this._inFlight.has(row.id)) continue;
      const eligible = this._nextEligible.get(row.id);
      if (eligible && eligible > now) continue;
      await this._dispatch(row);
    }
  }

  async _dispatch(row) {
    this._inFlight.add(row.id);
    try {
      switch (row.state) {
        case STATE.queuedUpload: {
          const bytes = await this._blob.get(row.id);
          if (!bytes) {
            await this._fail(row.id, "local blob missing for upload");
            return;
          }
          await this._adapter.upload(row.id, bytes, row.mediaType || "");
          await this._succeed(row.id);
          break;
        }
        case STATE.queuedDownload: {
          const bytes = await this._adapter.download(row.id);
          await this._blob.put(row.id, bytes);
          await this._succeed(row.id);
          break;
        }
        case STATE.queuedDelete: {
          await this._adapter.delete(row.id);
          await this._succeed(row.id);
          break;
        }
        default:
          break;
      }
    } catch (e) {
      await this._fail(row.id, String(e));
    } finally {
      this._inFlight.delete(row.id);
    }
  }

  async _succeed(id) {
    this._attempts.delete(id);
    this._lastErrors.delete(id);
    this._nextEligible.delete(id);
    const cur = await this._currentState(id);
    await this._gateway.patchState(id, onSuccess(cur));
  }

  async _fail(id, reason) {
    this._lastErrors.set(id, reason);
    const attempts = (this._attempts.get(id) ?? 0) + 1;
    this._attempts.set(id, attempts);
    if (attempts >= this._maxAttempts) {
      this._attempts.delete(id);
      this._nextEligible.delete(id);
      await this._gateway.patchState(id, STATE.archived);
      return;
    }
    const shift = Math.min(attempts, 6);
    const secs = Math.min(Math.max(1 << shift, 1), 60);
    this._nextEligible.set(id, new Date(this._now().getTime() + secs * 1000));
  }

  async _currentState(id) {
    try {
      return await this._gateway.currentState(id);
    } catch (_) {
      return STATE.synced;
    }
  }

  _newId() {
    // ponytail: timestamp+random id; swap for crypto.randomUUID() if a
    // measurement shows collisions (none expected at app scale).
    return `att_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
  }
}

module.exports = {
  ATTACHMENTS_TABLE,
  COL,
  STATE,
  QUEUED,
  DEFAULT_MAX_ATTEMPTS,
  onSuccess,
  SupabaseStorageAdapter,
  OpfsBlobStore,
  Attachments,
};
