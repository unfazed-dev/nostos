// SharedWorker broker for Flutter web (ADR-0051). Each tab gets a private
// MessagePort. The authenticated host page starts one DedicatedWorker for
// SQLite-WASM OPFS and transfers an engine port here. FileSystemSyncAccessHandle
// is unavailable in SharedWorkerGlobalScope.
// No bearer token or row payload travels over BroadcastChannel.
const clients = new Set();
const pending = new Map();
const sticky = new Map();
let engine = null;
let engineReady = false;
let scope = null;
let engineToken = null;
let engineTokenOwner = null;
let enginePaused = false;
let checkpoint = 0;
let nextId = -1;
let transition = Promise.resolve();
let wiping = false;
let sessionEpoch = 0;
let host = null;
let hostRequest = null;
let hostCandidate = null;
let recovery = null;

function reply(client, message) {
  try { client.port.postMessage(message); } catch (_) { clients.delete(client); }
}

function fail(client, id, error) {
  if (id !== undefined) reply(client, { id, error });
}

function sameScope(request) {
  if (!scope || (request.provider ?? "server") !== scope.provider ||
      request.url !== scope.url) return false;
  return scope.provider !== "appwrite" ||
    (request.projectId === scope.projectId &&
      request.functionId === scope.functionId && request.userId === scope.userId &&
      (request.gatewayUrl ?? null) === (scope.gatewayUrl ?? null));
}

function leaseDeadline(jwt) {
  const ceiling = Date.now() + 12 * 60 * 1000;
  try {
    const part = jwt.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    const exp = Number(JSON.parse(atob(part)).exp) * 1000;
    return Number.isFinite(exp) ? Math.min(exp, ceiling) : ceiling;
  } catch (_) {
    return ceiling;
  }
}

async function verifyAppwrite(request) {
  if (!request.token || !request.userId || !request.projectId || !request.url) return false;
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 8000);
  try {
    const response = await fetch(`${request.url.replace(/\/$/, "")}/account`, {
      method: "GET",
      credentials: "omit",
      cache: "no-store",
      headers: {
        "X-Appwrite-Project": request.projectId,
        "X-Appwrite-JWT": request.token,
        "Accept": "application/json",
      },
      signal: abort.signal,
    });
    if (!response.ok) return false;
    const account = await response.json();
    return account?.$id === request.userId && account?.status !== false;
  } catch (_) {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

function revoke(client) {
  if (client.timer) clearTimeout(client.timer);
  client.timer = null;
  client.authorized = false;
  client.token = null;
  client.expiresAt = 0;
  client.paused = false;
  reply(client, { type: "status", connected: false });
  for (const table of client.watched) {
    reply(client, { type: "snapshot", table, json: "[]" });
  }
  scheduleBearerHandoff(client);
}

function scheduleBearerHandoff(leaving) {
  if (scope?.provider !== "appwrite" || engineTokenOwner !== leaving || wiping) return;
  transition = transition.then(async () => {
    if (engineTokenOwner !== leaving || scope?.provider !== "appwrite") return;
    const candidates = [...clients].filter((client) => client !== leaving &&
      !client.closed && client.authorized && client.expiresAt > Date.now());
    for (const next of candidates) {
      if (!engineReady || recovery) await recoverEngine(next, next.token);
      else await askEngine({ cmd: "setToken", token: next.token });
      if (!allowed(next)) continue;
      engineTokenOwner = next;
      engineToken = next.token;
      return;
    }
    engineTokenOwner = null;
    engineToken = null;
    if (engineReady) await askEngine({ cmd: "disconnect" });
    enginePaused = true;
  }).catch(() => {
    engineTokenOwner = null;
    engineToken = null;
  });
}

function grant(client, token) {
  if (client.timer) clearTimeout(client.timer);
  client.authorized = true;
  client.token = token ?? null;
  client.expiresAt = scope.provider === "appwrite" ? leaseDeadline(token) : Infinity;
  const delay = client.expiresAt - Date.now();
  client.timer = Number.isFinite(delay)
    ? setTimeout(() => revoke(client), Math.max(1, delay)) : null;
}

function allowed(client) {
  if (client.closed) return false;
  if (client.authorized && client.expiresAt > Date.now()) return true;
  if (client.authorized) revoke(client);
  return false;
}

function engineMessage(message) {
    if (message.id !== undefined && pending.has(message.id)) {
      const p = pending.get(message.id);
      pending.delete(message.id);
      clearTimeout(p.timer);
      if (message.error) p.reject(new Error(message.error));
      else p.resolve(message);
      return;
    }
    if (!message.type) return;
    if (["storage", "status", "writeStatus"].includes(message.type)) {
      sticky.set(message.type, message);
    }
    if (message.type === "storage") {
      for (const client of clients) reply(client, message);
      return;
    }
    for (const client of clients) {
      if (!allowed(client) || client.paused) continue;
      if (message.type === "snapshot" && !client.watched.has(message.table)) continue;
      reply(client, message);
    }
}

function releaseEngine() {
  for (const p of pending.values()) {
    clearTimeout(p.timer);
    p.reject(new Error("browser sync engine stopped"));
  }
  pending.clear();
  if (host && !host.closed) reply(host, { type: "retireEngine" });
  engine?.close();
  engine = null;
  engineReady = false;
  host = null;
  engineTokenOwner = null;
  enginePaused = false;
}

function ensureEngine(client) {
  if (engine) return Promise.resolve();
  if (hostRequest) return hostRequest.promise;
  if (!client || client.closed) return Promise.reject(new Error("browser sync host unavailable"));
  let resolveHost;
  let rejectHost;
  const promise = new Promise((resolve, reject) => {
    resolveHost = resolve;
    rejectHost = reject;
  });
  const timer = setTimeout(() => {
    hostRequest = null;
    rejectHost(new Error("browser sync host did not attach"));
  }, 10000);
  hostRequest = { client, promise, resolveHost, rejectHost, timer };
  reply(client, { type: "needEngine" });
  return promise;
}

function attachEngine(client, message) {
  if (hostRequest?.client !== client || !message.port || client.closed) {
    return fail(client, message.id, "browser sync host was not requested");
  }
  clearTimeout(hostRequest.timer);
  engine = message.port;
  engineReady = false;
  host = client;
  engine.onmessage = ({ data }) => engineMessage(data);
  engine.onmessageerror = () => releaseEngine();
  engine.start();
  hostRequest.resolveHost();
  hostRequest = null;
}

async function askEngine(message, timeoutMs = 30000) {
  await ensureEngine(hostCandidate ?? [...clients].find((client) => allowed(client)));
  const id = nextId--;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      releaseEngine();
      reject(new Error("browser sync engine timed out"));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
    engine.postMessage({ ...message, id });
  });
}

function sendSticky(client) {
  for (const message of sticky.values()) reply(client, message);
}

function sendWatches(client) {
  for (const table of client.watched) engine.postMessage({ cmd: "watch", table });
}

async function recoverEngine(client, token) {
  while (recovery) {
    const previous = recovery;
    try { await previous; } catch (_) { /* retry with this verified bearer */ }
    if (engineReady) return;
  }
  if (engineReady) return;
  recovery = (async () => {
    if (!scope || client.closed) throw new Error("browser sync session ended");
    const epoch = sessionEpoch;
    hostCandidate = client;
    try {
      for (let attempt = 0; attempt < 12; attempt++) {
        try {
          if (client.schema) await askEngine(client.schema);
          if (client.crdt) await askEngine(client.crdt);
          const result = await askEngine({ cmd: "connect", ...scope, token,
            tables: [], orSetTables: [], counterTables: [] });
          if (client.closed || epoch !== sessionEpoch || !scope) {
            throw new Error("browser sync session ended");
          }
          if (scope.provider === "appwrite" &&
              (leaseDeadline(token) <= Date.now() ||
                (client.joinedEpoch === epoch && !allowed(client)))) {
            throw new Error("browser sync lease expired during recovery");
          }
          checkpoint = result.checkpoint ?? checkpoint;
          engineReady = true;
          engineToken = token;
          engineTokenOwner = client;
          enginePaused = false;
          if (client.paused && ![...clients].some((other) =>
            other !== client && allowed(other) && !other.paused)) {
            await askEngine({ cmd: "disconnect" });
            enginePaused = true;
          }
          for (const other of clients) if (allowed(other)) sendWatches(other);
          return;
        } catch (error) {
          if (!String(error.message).includes("another browser tab owns durable storage") ||
              attempt === 11) throw error;
          releaseEngine();
          await new Promise((resolve) => setTimeout(resolve, 250));
        }
      }
    } finally {
      hostCandidate = null;
    }
  })();
  try { await recovery; }
  catch (error) { releaseEngine(); throw error; }
  finally { recovery = null; }
}

async function connect(client, message) {
  const id = message.id;
  if (client.closed) return;
  if (wiping) return fail(client, id, "browser storage is being cleared");
  if (scope && !sameScope(message)) {
    return fail(client, id, "another signed-in session owns browser storage");
  }
  if ((message.provider ?? "server") === "appwrite") {
    if (!(await verifyAppwrite(message))) {
      return fail(client, id, "another signed-in session owns browser storage");
    }
  } else if (scope && (message.token ?? null) !== engineToken) {
    return fail(client, id, "another signed-in session owns browser storage");
  }
  // A sign-out may have completed while the network account check was in
  // flight. `transition` serializes all connect and sign-out boundaries.
  if (scope && !sameScope(message)) {
    return fail(client, id, "another signed-in session owns browser storage");
  }
  if (client.closed) return;
  hostCandidate = client;
  try {
    if (scope && !engineReady) {
      await recoverEngine(client, message.token);
    } else if (!scope) {
      let result;
      for (let attempt = 0; attempt < 12; attempt++) {
        try {
          if (client.schema) await askEngine(client.schema);
          if (client.crdt) await askEngine(client.crdt);
          result = await askEngine(message);
          break;
        } catch (error) {
          if (!String(error.message).includes("another browser tab owns durable storage") ||
              attempt === 11) throw error;
          releaseEngine();
          await new Promise((resolve) => setTimeout(resolve, 250));
        }
      }
      if ((message.provider ?? "server") === "appwrite" &&
          leaseDeadline(message.token) <= Date.now()) {
        throw new Error("browser sync lease expired during connect");
      }
      scope = {
        provider: message.provider ?? "server", url: message.url,
        projectId: message.projectId, functionId: message.functionId,
        gatewayUrl: message.gatewayUrl ?? null,
        userId: message.userId,
      };
      engineReady = true;
      engineToken = message.token ?? null;
      engineTokenOwner = client;
      enginePaused = false;
      checkpoint = result.checkpoint ?? 0;
    } else if (scope.provider === "appwrite" && !engineTokenOwner) {
      if (client.schema) await askEngine(client.schema);
      if (client.crdt) await askEngine(client.crdt);
      // The last lease may have expired and disconnected the shared engine.
      // A newly verified account restores its bearer before reading rows.
      await askEngine({ cmd: "setToken", token: message.token });
      await askEngine({ cmd: "resume" });
      engineToken = message.token;
      engineTokenOwner = client;
      enginePaused = false;
    } else {
      if (client.schema) await askEngine(client.schema);
      if (client.crdt) await askEngine(client.crdt);
      if (enginePaused) {
        if (scope.provider === "appwrite") {
          await askEngine({ cmd: "setToken", token: message.token });
          engineToken = message.token;
          engineTokenOwner = client;
        }
        await askEngine({ cmd: "resume" });
        enginePaused = false;
      }
    }
    if (client.closed) {
      if (![...clients].some((other) => other.authorized)) {
        releaseEngine();
        scope = null;
        engineToken = null;
        sticky.clear();
      } else if (engineTokenOwner === client) {
        scheduleBearerHandoff(client);
      }
      return;
    }
    grant(client, message.token);
    client.joinedEpoch = sessionEpoch;
    reply(client, { id, ok: true, checkpoint });
    sendSticky(client);
    sendWatches(client);
  } catch (error) {
    if (!engineReady) releaseEngine();
    fail(client, id, String(error.message).slice(0, 300));
  } finally {
    hostCandidate = null;
  }
}

async function rotate(client, message) {
  if ((!allowed(client) && client.joinedEpoch !== sessionEpoch) || !scope || wiping) {
    return fail(client, message.id, "browser storage session not authorized");
  }
  const candidate = message.token ?? null;
  if (scope.provider === "appwrite" &&
      !(await verifyAppwrite({ ...scope, token: candidate }))) {
    revoke(client);
    return fail(client, message.id, "Appwrite token does not match the active account");
  }
  if (client.closed || wiping || client.joinedEpoch !== sessionEpoch) {
    return fail(client, message.id, "browser storage session not authorized");
  }
  try {
    if (!engineReady || recovery) await recoverEngine(client, candidate);
    const reconnect = !engineTokenOwner && !client.paused;
    const result = await askEngine(message);
    if (client.closed || wiping || client.joinedEpoch !== sessionEpoch) {
      return fail(client, message.id, "browser storage session ended");
    }
    if (reconnect) await askEngine({ cmd: "resume" });
    if (reconnect) enginePaused = false;
    engineToken = candidate;
    engineTokenOwner = client;
    grant(client, candidate);
    sendSticky(client);
    sendWatches(client);
    if (scope.provider !== "appwrite") {
      for (const other of clients) if (other !== client) revoke(other);
    }
    reply(client, { ...result, id: message.id });
  } catch (error) {
    if (!engineReady) releaseEngine();
    fail(client, message.id, String(error.message).slice(0, 300));
  }
}

async function signOut(client, message) {
  if (client.closed || client.joinedEpoch !== sessionEpoch || !scope) {
    return fail(client, message.id, "browser storage session not authorized");
  }
  wiping = true;
  try {
    if (recovery) {
      try { await recovery; } catch (_) { /* local wipe still proceeds */ }
    }
    // A revoked tab is still allowed to wipe its own joined session. It can
    // host a blank Worker that opens OPFS without an online account check.
    hostCandidate = client;
    try {
      await askEngine({ cmd: "signOut" }, 20000);
    } catch (error) {
      // A silent Worker death is detected by the request timeout. Its port is
      // retired then; a fresh blank Worker can still wipe OPFS offline.
      if (engine) throw error;
      await askEngine({ cmd: "signOut" }, 20000);
    }
    scope = null;
    engineToken = null;
    engineTokenOwner = null;
    checkpoint = 0;
    sessionEpoch++;
    sticky.delete("status");
    sticky.delete("writeStatus");
    for (const other of clients) revoke(other);
    releaseEngine();
    reply(client, { id: message.id, ok: true });
  } catch (error) {
    if (!engineReady || String(error.message).includes("browser storage unavailable for local wipe")) {
      releaseEngine();
    }
    fail(client, message.id, String(error.message).slice(0, 300));
  } finally {
    hostCandidate = null;
    wiping = false;
  }
}

async function forward(client, message) {
  if (!allowed(client) || wiping) {
    return fail(client, message.id, "browser storage session not authorized");
  }
  const epoch = sessionEpoch;
  try {
    if (!engineReady || recovery) await recoverEngine(client, client.token);
    const result = await askEngine(message);
    if (wiping || epoch !== sessionEpoch || !allowed(client)) {
      return fail(client, message.id, "browser storage session ended");
    }
    reply(client, { ...result, id: message.id });
  } catch (error) {
    fail(client, message.id, String(error.message).slice(0, 300));
  }
}

function close(client, message) {
  const wasHost = host === client;
  clients.delete(client);
  client.closed = true;
  if (client.timer) clearTimeout(client.timer);
  client.authorized = false;
  client.token = null;
  reply(client, { id: message.id, ok: true });
  if (!wasHost) scheduleBearerHandoff(client);
  if (wasHost) releaseEngine();
  if (![...clients].some((other) => other.authorized)) {
    releaseEngine();
    scope = null;
    engineToken = null;
    checkpoint = 0;
    sessionEpoch++;
    sticky.clear();
  } else if (wasHost) {
    // A surviving same-account tab supplies the next OPFS-capable worker.
    // It already has a validated lease, but the new dedicated worker checks
    // its JWT again before reopening cached rows.
    transition = transition.then(() => promote()).catch(() => {});
  }
}

async function promote() {
  const next = [...clients].find((client) => allowed(client));
  if (!next || !scope) return;
  try {
    await recoverEngine(next, next.token);
  } catch (_) {
    releaseEngine();
    for (const client of clients) revoke(client);
    scope = null;
    engineToken = null;
    sticky.clear();
  }
}

function handle(client, message) {
  switch (message.cmd) {
    case "attachEngine":
      attachEngine(client, message);
      break;
    case "applySchema":
      client.schema = message;
      if (allowed(client)) void forward(client, message);
      else reply(client, { id: message.id, ok: true });
      break;
    case "setCrdtTables":
      client.crdt = message;
      if (allowed(client)) void forward(client, message);
      else reply(client, { id: message.id, ok: true });
      break;
    case "watch":
      client.watched.add(message.table);
      if (allowed(client) && engineReady) engine.postMessage(message);
      break;
    case "unwatch":
      client.watched.clear();
      break;
    case "connect":
      transition = transition.then(() => connect(client, message)).catch((error) =>
        fail(client, message.id, String(error.message).slice(0, 300)));
      break;
    case "setToken":
      transition = transition.then(() => rotate(client, message)).catch((error) =>
        fail(client, message.id, String(error.message).slice(0, 300)));
      break;
    case "signOut":
      transition = transition.then(() => signOut(client, message)).catch((error) =>
        fail(client, message.id, String(error.message).slice(0, 300)));
      break;
    case "close":
      close(client, message);
      break;
    case "disconnect":
      transition = transition.then(async () => {
        if (!allowed(client)) return fail(client, message.id, "browser storage session not authorized");
        client.paused = true;
        if (engineReady && ![...clients].some((other) => allowed(other) && !other.paused)) {
          await askEngine({ cmd: "disconnect" });
          enginePaused = true;
        }
        reply(client, { id: message.id, ok: true });
        reply(client, { type: "status", connected: false });
      }).catch((error) => fail(client, message.id, String(error.message).slice(0, 300)));
      break;
    case "resume":
      transition = transition.then(async () => {
        if (!allowed(client)) return fail(client, message.id, "browser storage session not authorized");
        client.paused = false;
        if (!engineReady || recovery) await recoverEngine(client, client.token);
        await askEngine(message);
        enginePaused = false;
        reply(client, { id: message.id, ok: true });
        sendSticky(client);
        sendWatches(client);
      }).catch((error) => fail(client, message.id, String(error.message).slice(0, 300)));
      break;
    default:
      void forward(client, message);
  }
}

self.onconnect = ({ ports }) => {
  const client = {
    port: ports[0], authorized: false, token: null, expiresAt: 0,
    timer: null, watched: new Set(), schema: null, crdt: null, paused: false,
    closed: false, joinedEpoch: -1,
  };
  clients.add(client);
  client.port.onmessage = ({ data }) => handle(client, data || {});
  client.port.start();
  if (sticky.has("storage")) reply(client, sticky.get("storage"));
};
