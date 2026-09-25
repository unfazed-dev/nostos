// Live-replication E2E for the nostos_node SDK against the shared spine server
// (`nostos-infra/examples/e2e_server`). Proves the SDK's real public API drives
// a full server→client→server round-trip with NO Postgres and NO docker — the
// Node mirror of `crates/nostos-client/tests/e2e_live_replication.rs`.
//
// 1. **PUSH**: `POST /push` injects a `tasks` row server-side → it replicates
//    over the real WS → the SDK applies it → `query()` reads it.
// 2. **ECHO**: the SDK `write()`s a row to its durable outbox → the server's
//    echo `WriteBack` accepts + re-emits it through the fan-out → the writer
//    receives its own write → `query()` reads it.
//
// Run: `node smoke_live.cjs` (after `cargo build --release`).
// Requires Node 18+ for global `fetch`.
'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn, execFileSync } = require('child_process');

// ---- locate the nostos workspace root (the shared `target/` lives there) ----
// Walks up from this file's dir until it finds a `target/` dir + a
// `crates/nostos-infra` member — that's the nostos workspace root.
function workspaceRoot() {
  let dir = __dirname;
  for (let i = 0; i < 8; i++) {
    if (
      fs.existsSync(path.join(dir, 'target')) &&
      fs.existsSync(path.join(dir, 'crates', 'nostos-infra'))
    ) {
      return dir;
    }
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  throw new Error(`could not locate nostos workspace root from ${__dirname}`);
}

// ---- locate the spine binary, building it if absent (mirrors the Rust template) ----
function spineBinaryPath(root) {
  const candidate = path.join(root, 'target', 'debug', 'examples', 'e2e_server');
  if (fs.existsSync(candidate)) return candidate;
  console.log('[node-e2e] spine not found; building nostos-infra example...');
  execFileSync(
    'cargo',
    ['build', '-p', 'nostos-infra', '--example', 'e2e_server'],
    { cwd: root, stdio: 'inherit' },
  );
  if (!fs.existsSync(candidate)) {
    throw new Error(`spine binary still missing at ${candidate} after build`);
  }
  return candidate;
}

// ---- spawn the spine, discover its port via stdout lines ----
function spawnSpine(exe) {
  return new Promise((resolve, reject) => {
    const child = spawn(exe, [], { stdio: ['ignore', 'pipe', 'inherit'] });
    let port = null;
    let settled = false;

    const readyTimer = setTimeout(() => {
      if (!settled) {
        settled = true;
        reject(new Error('spine never reached NOSTOS_E2E_READY within 30s'));
      }
    }, 30000);

    let buf = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      buf += chunk;
      let idx;
      while ((idx = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, idx).trim();
        buf = buf.slice(idx + 1);
        if (line.startsWith('NOSTOS_E2E_PORT=')) {
          port = Number.parseInt(line.slice('NOSTOS_E2E_PORT='.length), 10);
        }
        if (line === 'NOSTOS_E2E_READY' && port != null && !settled) {
          settled = true;
          clearTimeout(readyTimer);
          // Leave the stdout listener attached so the spine isn't SIGPIPE'd.
          resolve({ child, port });
        }
      }
    });

    child.on('error', (err) => {
      if (!settled) {
        settled = true;
        clearTimeout(readyTimer);
        reject(new Error(`failed to spawn spine: ${err.message}`));
      }
    });
    child.on('exit', (code, signal) => {
      if (!settled) {
        settled = true;
        clearTimeout(readyTimer);
        reject(new Error(`spine exited before READY: code=${code} signal=${signal}`));
      }
    });
  });
}

// ---- poll query() until at least one row appears, or the deadline elapses ----
async function pollRow(client, sql, timeoutMs) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    const rowsJson = await client.query(sql);
    const rows = JSON.parse(rowsJson);
    if (Array.isArray(rows) && rows.length > 0) return rows;
    await new Promise((r) => setTimeout(r, 100));
  }
  return null;
}

// ---- HTTP POST /push via Node 18+ global fetch ----
async function httpPush(port, body) {
  const res = await fetch(`http://127.0.0.1:${port}/push`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body,
  });
  if (!res.ok) throw new Error(`POST /push non-200: ${res.status}`);
}

async function main() {
  const root = workspaceRoot();
  const spineExe = spineBinaryPath(root);
  console.log(`[node-e2e] spine binary: ${spineExe}`);
  const { child, port } = await spawnSpine(spineExe);
  console.log(`[node-e2e] spine ready on port ${port}`);

  // Load the built addon (same load path as smoke.cjs).
  const addonPath = path.join(__dirname, 'nostos_node.node');
  const addon = require(addonPath);
  const NostosClient = addon.NostosClient;
  if (typeof NostosClient !== 'function') {
    throw new Error('NostosClient class missing from addon exports');
  }

  const wsUrl = `ws://127.0.0.1:${port}/sync`;
  // PID-unique DB path so a stale file from a prior run can't false-positive.
  const dbPath = path.join(os.tmpdir(), `nostos-node-e2e-${process.pid}.sqlite`);
  try { fs.unlinkSync(dbPath); } catch { /* fine if absent */ }

  const client = new NostosClient(wsUrl, null, dbPath);
  console.log(`[node-e2e] client url=${client.url}`);

  let exitCode = 1;
  try {
    // subscribe() does what connect() does (opens storage, builds SyncClient)
    // AND spawns the run_with_reconnect loop on the owned runtime. Calling
    // connect() first would create a session that subscribe() immediately
    // drops — strictly wasteful — so we go straight to subscribe(), mirroring
    // the Rust reference template (which spawns run_once with no separate
    // connect step).
    await client.subscribe('tasks', null);
    console.log('[node-e2e] subscribed to tasks');
    // Let the subscribe land + the session register with the fan-out service
    // (the spine only delivers to sessions registered at fan-out time).
    await new Promise((r) => setTimeout(r, 500));

    // ---- direction 1: server PUSH → on-device query ----
    const pushBody = JSON.stringify({
      pk: 'node-push',
      payload: { title: 'from-server', status: 'open', priority: '5' },
    });
    await httpPush(port, pushBody);
    console.log('[node-e2e] POST /push ok');

    const pushed = await pollRow(
      client,
      "SELECT pk FROM nostos_data WHERE table_name = 'tasks' AND pk = 'node-push'",
      8000,
    );
    if (!pushed) throw new Error('pushed row never became queryable');
    if (pushed[0].pk !== 'node-push') {
      throw new Error(`unexpected pushed pk: ${JSON.stringify(pushed[0])}`);
    }
    console.log('[node-e2e] PUSH_OK: %j', pushed[0]);

    // ---- direction 2: client WRITE → server echo → on-device query ----
    const payloadJson = JSON.stringify({
      title: 'from-client',
      status: 'open',
      priority: '5',
    });
    await client.write('tasks', 'upsert', 'node-echo', payloadJson);
    console.log('[node-e2e] write() enqueued (node-echo)');

    const echoed = await pollRow(
      client,
      "SELECT pk FROM nostos_data WHERE table_name = 'tasks' AND pk = 'node-echo'",
      8000,
    );
    if (!echoed) throw new Error('echoed write never became queryable');
    if (echoed[0].pk !== 'node-echo') {
      throw new Error(`unexpected echoed pk: ${JSON.stringify(echoed[0])}`);
    }
    console.log('[node-e2e] ECHO_OK: %j', echoed[0]);

    try { await client.close(); } catch { /* best-effort teardown */ }
    console.log('[node-e2e] DONE');
    exitCode = 0;
  } catch (e) {
    console.error('[node-e2e] FAIL:', e && e.stack ? e.stack : e);
    try { await client.close(); } catch { /* best-effort */ }
  } finally {
    try { child.kill('SIGTERM'); } catch { /* already gone */ }
    try { fs.unlinkSync(dbPath); } catch { /* absent is fine */ }
  }
  process.exit(exitCode);
}

main();
