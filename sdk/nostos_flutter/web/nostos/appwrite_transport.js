// Appwrite Cloud HTTP transport for the shared Rust NostosEngine.
// The Worker owns scheduling and JWT memory; Rust owns local identity, cursor,
// row apply, mutation IDs and the durable outbox (ADR-0050).
import { NostosEngine } from "./nostos_ffi_wasm.js";

const PAGE_LIMIT = 100;
const MAX_PAGES = 64;
const RETRY_MS = 5000;

function functionError(status, body) {
  const message = String(body?.message ?? body?.error ?? "unknown").slice(0, 300);
  const error = new Error(`Appwrite HTTP ${status}: ${message}`);
  error.status = status;
  error.body = message;
  return error;
}

export class AppwriteTransport {
  constructor({ dbHandle, endpoint, projectId, functionId, userId, token, onStatus }) {
    if (!token || !userId) throw new Error("Appwrite session required");
    this.engine = NostosEngine.newAppwrite(dbHandle, dbHandle?.deviceId ?? crypto.randomUUID());
    this.engine.bindAppwritePrincipal(endpoint, projectId, functionId, userId);
    this.endpoint = endpoint.replace(/\/$/, "");
    this.projectId = projectId;
    this.functionId = functionId;
    this.token = token;
    this.onStatus = onStatus;
    this.change = null;
    this.paused = false;
    this.closed = false;
    this.inFlight = false;
    this.connected = false;
    this.activeRequest = null;
    this.lastSyncError = null;
    this.timer = setInterval(() => void this.sync(), RETRY_MS);
    queueMicrotask(() => void this.sync());
  }

  get checkpoint() { return Number(this.engine.appwriteAfter); }
  get pendingCount() { return this.engine.pendingCount; }
  get deadLetteredCount() { return this.engine.deadLetteredCount; }
  get lastError() { return this.engine.lastError ?? this.lastSyncError; }
  rowsFor(table) { return this.engine.rowsFor(table); }
  query(sql) { return this.engine.query(sql); }
  applySchema(tables) { this.engine.applySchema(tables); this.tick(); }
  setCrdtTables(orSet, counter) { this.engine.setCrdtTables(orSet, counter); }
  onChange(callback) { this.change = callback; this.tick(); }
  offChange() { this.change = null; }
  tick() { this.change?.(); }

  write(table, op, pk, payloadJson) {
    return this.writeBatch([{ table, op, pk, payloadJson }])[0];
  }
  writeBatch(ops) {
    const ids = Array.from(this.engine.writeBatch(ops));
    this.tick();
    queueMicrotask(() => void this.sync());
    return ids;
  }
  orSetAdd(table, pk, element) { return this.engine.orSetAdd(table, pk, element); }
  orSetRemove(table, pk, element) { return this.engine.orSetRemove(table, pk, element); }
  counterIncrement(table, pk, delta) { return this.engine.counterIncrement(table, pk, delta); }
  counterDecrement(table, pk, delta) { return this.engine.counterDecrement(table, pk, delta); }

  setToken(token) {
    this.token = token;
    if (token) queueMicrotask(() => void this.sync());
  }
  disconnect() {
    this.paused = true;
    this.activeRequest?.abort();
    this.connected = false;
    this.onStatus(false);
  }
  resume() {
    this.paused = false;
    queueMicrotask(() => void this.sync());
  }
  close() {
    this.closed = true;
    this.paused = true;
    this.token = null;
    this.activeRequest?.abort();
    clearInterval(this.timer);
  }
  clearLocalState() {
    this.close();
    this.engine.appwriteRevoke();
    this.tick();
  }

  async call(path, body) {
    if (this.closed || this.paused || !this.token) throw new Error("Appwrite session paused");
    const controller = new AbortController();
    this.activeRequest = controller;
    try {
      const response = await fetch(`${this.endpoint}/functions/${this.functionId}/executions`, {
        method: "POST",
        signal: controller.signal,
        headers: {
          "Content-Type": "application/json",
          "X-Appwrite-Project": this.projectId,
          "X-Appwrite-JWT": this.token,
        },
        body: JSON.stringify({ method: "POST", path, body: JSON.stringify(body) }),
      });
      if (this.closed || this.paused) throw new Error("Appwrite session paused");
      const envelope = await response.json();
      if (this.closed || this.paused) throw new Error("Appwrite session paused");
      if (!response.ok) throw functionError(response.status, envelope);
      const status = envelope.responseStatusCode;
      if (!Number.isInteger(status)) throw new Error("Appwrite Function status missing");
      let result;
      try { result = JSON.parse(envelope.responseBody ?? "null"); }
      catch (_) { throw new Error("Appwrite Function body invalid"); }
      if (status < 200 || status >= 300) throw functionError(status, result);
      return result;
    } finally {
      if (this.activeRequest === controller) this.activeRequest = null;
    }
  }

  async sync() {
    if (this.closed || this.paused || this.inFlight || !this.token) return;
    this.inFlight = true;
    try {
      for (const write of JSON.parse(this.engine.appwritePending())) {
        const { id, ...body } = write;
        try {
          await this.call("/sync/push", body);
          if (this.closed || this.paused) return;
          this.engine.appwriteAck(id);
          this.tick();
        } catch (error) {
          if (this.closed || this.paused) return;
          if (error.status === 403 && error.body === "account inactive") throw error;
          const permanent = error.status !== undefined &&
            ![401, 409, 429].includes(error.status) && error.status < 500;
          this.engine.appwriteReject(id, String(error.message).slice(0, 300), permanent);
          this.tick();
          if (!permanent) throw error;
        }
      }
      for (let i = 0; i < MAX_PAGES; i++) {
        const body = await this.call("/sync/pull", {
          after: this.engine.appwriteAfter,
          limit: PAGE_LIMIT,
        });
        if (this.closed || this.paused) return;
        const outcome = JSON.parse(this.engine.applyAppwritePage(JSON.stringify(body)));
        this.tick();
        if (outcome.resnapshot) continue;
        if (!outcome.has_more) break;
      }
      if (!this.closed && !this.paused) {
        this.lastSyncError = null;
        this.connected = true;
        this.onStatus(true);
      }
    } catch (error) {
      if (this.closed || this.paused) return;
      if (error.status === 403 && error.body === "account inactive") {
        this.engine.appwriteRevoke();
        this.token = null;
        this.paused = true;
        this.tick();
        this.onStatus(false, true);
      } else {
        this.lastSyncError = String(error.message).slice(0, 300);
        this.connected = false;
        this.tick();
        this.onStatus(false);
      }
    } finally {
      this.inFlight = false;
    }
  }
}
