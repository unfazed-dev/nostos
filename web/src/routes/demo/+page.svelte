<script lang="ts">
  import { onMount } from 'svelte';
  import { Nostos, Button } from '$lib/components/ui';
  // Lazy/dynamic import: adapter-static prerenders the site, and the wasm
  // module must not be pulled into the top-level graph (it references
  // `import.meta.url` + fetches a .wasm at runtime). Loading it inside onMount
  // keeps the static export clean (gate 4). The default export is the async
  // `init()` (wasm-bindgen convention); named exports are the classes.
  import type { NostosSocket, RowEntry } from 'nostos-ffi-wasm';

  /**
   * /demo — the moat, visible in a browser tab.
   *
   * One WebSocket to the dev-stack server (`make dev-stack`), a `where_sql`
   * predicate input, the live filtered rows, and the checkpoint LSN ticking
   * forward. Reload the tab → it resumes from the persisted checkpoint.
   *
   * Architecture (read this before editing): the page owns NOTHING of the
   * transport. `NostosSocket.connect(...)` (from nostos-ffi-wasm) owns the
   * WebSocket, the engine, the apply loop, the ack, AND the localStorage
   * checkpoint persistence. This page only:
   *   1. Calls `NostosSocket.connect(url, token, table, whereSql)`.
   *   2. Polls `socket.checkpoint` / `socket.rowCount` / `socket.rowsFor(table)`
   *      on a render tick (every 250ms while live) to drive the UI.
   * That's it. There is no decode/apply/ack/persist logic here — the ~30 lines
   * of WS glue + the engine's atomic apply + the idempotency + the resume all
   * live in `NostosSocket`, which IS the WS-glue manual check ADR-0017 calls out
   * ("WS glue untested in CI; covered by the E3 demo page manual check").
   *
   * ## Why current-state, not event-log (the design choice)
   *
   * The previous version rendered an append-only event log (each inbound frame
   * → a row in the list). `NostosSocket` has no per-frame callback hook (its
   * WS closures are private, kept-alive-only fields), and adding one is scope
   * creep beyond the readback fix. So this page renders the engine's
   * *current state*: `rowsFor(table)` on a tick → the rows that match the
   * predicate RIGHT NOW. The op/lsn history is dropped — honest to what the
   * engine knows ("these are the matching rows"), and a better demo than a raw
   * event feed. The live `rowCount` + advancing `checkpoint` already show
   * activity; the row list shows the filtered table state.
   * ponytail: if a future event-feed view is wanted, add a per-frame callback
   * to `NostosSocket` (a JS-side `onframe` setter) rather than reviving the
   * duplicated WS glue here.
   *
   * ## localStorage checkpoint
   *
   * `NostosSocket` persists the checkpoint to `localStorage[cairn:checkpoint:<table>]`
   * internally on every ack + on close (verified in nostos-ffi-wasm/src/transport.rs).
   * The socket does NOT expose the *resumed-from* value back to JS, so for the
   * "resumed from N" subtitle this page reads the key once on connect
   * (display-only — the socket is the single writer). If the key is absent,
   * resume is 0 (full snapshot).
   */

  // ---- connection config (editable; the verified dev-stack default is 8800) ----
  // The repo's `nostos-server` defaults NOSTOS_BIND to 0.0.0.0:8800 (see
  // crates/nostos-server/src/main.rs). The plan text said 8080; that is wrong
  // against the code — 8800 is what `make dev-stack` actually binds. Made
  // editable so an operator on a different port need not touch the source.
  let url = $state('ws://localhost:8800/sync');
  let table = $state('tasks');
  // The dev `tasks` schema (docker/pg-init/01-sources.sql) has NO `priority` or
  // `status` column — only id/org_id/assignee_id/title/completed/created_at/
  // updated_at. So the examples below reference REAL columns. The grammar is
  // the safe-SQL subset: 6 comparison ops + AND/OR/NOT + parens
  // (crates/nostos-domain/src/predicate_compile.rs). Empty = match-all.
  let whereSql = $state('completed = false');
  let token = $state('');

  // ---- live state ----
  type Status = 'idle' | 'connecting' | 'live' | 'closed' | 'error';
  let status = $state<Status>('idle');
  let err = $state<string | null>(null);

  // A rendered row: pk + the decoded tuple image (we attempt JSON, fall back to
  // the raw hex). The engine stores opaque bytes; `rowsFor` hands us the
  // `(pk, Uint8Array)` pairs and we decode for display only.
  type Row = { pk: string; payload: Record<string, unknown> | string };
  let rows = $state<Row[]>([]);
  let checkpoint = $state(0);
  let rowCount = $state(0);
  let resumedFrom = $state<number | null>(null);

  let socket: NostosSocket | null = null;
  let initMod: typeof import('nostos-ffi-wasm') | null = null;
  // The render tick: re-reads the socket's state on an interval while live.
  let tickHandle: ReturnType<typeof setInterval> | null = null;

  /** The localStorage key NostosSocket persists the checkpoint under. */
  const checkpointKey = () => `cairn:checkpoint:${table}`;

  /**
   * Decode a row's opaque payload bytes to a displayable value: the
   * PgReplicator emits a small JSON object {col:val} (see the extractor in
   * crates/nostos-server/src/main.rs); try that first, fall back to a hex string
   * on any failure. The apply engine itself never inspects payload contents —
   * this is display-only.
   */
  function decodePayload(bytes: Uint8Array): Record<string, unknown> | string {
    try {
      const text = new TextDecoder().decode(bytes);
      return JSON.parse(text) as Record<string, unknown>;
    } catch {
      // Fallback: render the raw bytes as hex so the operator at least sees
      // something distinguishing per row.
      return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
    }
  }

  /**
   * Read the engine's current state through the socket's readback API and
   * refresh the rendered rows. Called on the render tick while live. We snapshot
   * checkpoint + rowCount alongside so the stats panel and the row list stay in
   * sync (no half-updated frame mid-tick).
   */
  function snapshot() {
    if (!socket) return;
    checkpoint = socket.checkpoint;
    rowCount = socket.rowCount;
    const entries: RowEntry[] = socket.rowsFor(table);
    rows = entries.map((e) => ({ pk: e.pk, payload: decodePayload(e.payload) }));
  }

  /**
   * Read the persisted checkpoint for display only ("resumed from N"). The
   * socket owns persistence; this is a read-only peek at the same key
   * `NostosSocket` writes (nostos-ffi-wasm/src/transport.rs::checkpoint_key).
   * Returns 0 if the key is absent/malformed (matches the socket's resume-fallback).
   */
  function readPersistedCheckpoint(): number {
    const raw = localStorage.getItem(checkpointKey());
    const n = raw ? parseInt(raw, 10) : NaN;
    return Number.isFinite(n) ? n : 0;
  }

  async function connect() {
    err = null;
    status = 'connecting';
    try {
      // Lazily init the wasm module on first connect (not at module load — keeps
      // adapter-static's prerender clean).
      if (!initMod) {
        initMod = await import('nostos-ffi-wasm');
        await initMod.default();
      }

      // Snapshot the resume point for the "resumed from N" subtitle BEFORE
      // connect — NostosSocket reads + sends resume_lsn internally; this read is
      // display-only.
      resumedFrom = readPersistedCheckpoint();

      // NostosSocket owns the WS handshake, the subscribe frame (with resume_lsn
      // + where_sql), the apply loop, the ack, AND the localStorage checkpoint
      // persistence. This page does none of that.
      socket = await initMod.NostosSocket.connect(
        url,
        token || null,
        table,
        whereSql.trim() || null
      );
      status = 'live';

      // Prime the view immediately (rows may already have arrived between the
      // socket's OPEN and this await resolving), then poll on a tick.
      snapshot();
      tickHandle = setInterval(snapshot, 250);
    } catch (e) {
      status = 'error';
      err = (e as Error).message ?? String(e);
      stopTick();
    }
  }

  function stopTick() {
    if (tickHandle !== null) {
      clearInterval(tickHandle);
      tickHandle = null;
    }
  }

  function disconnect() {
    socket?.close();
    socket = null;
    stopTick();
    // No final snapshot here: socket.close() triggers the onclose flush
    // asynchronously (the closure runs on a later tick, after `socket = null`),
    // so a snapshot now wouldn't reflect the final flush. The next `connect()`
    // re-seeds `rows` from the engine's current state.
    status = 'closed';
  }

  /** Wipe the persisted checkpoint (start completely fresh on next connect). */
  function reset() {
    localStorage.removeItem(checkpointKey());
    rows = [];
    checkpoint = 0;
    rowCount = 0;
    resumedFrom = null;
  }

  onMount(() => () => {
    disconnect();
    stopTick();
  });
</script>

<svelte:head>
  <title>Nostos · Live Demo</title>
</svelte:head>

<section class="demo">
  <header class="demo-head">
    <a class="brand" href="/"><Nostos size={20} /> <span>Nostos</span></a>
    <span class="sync-pill" class:live={status === 'live'}>
      <span class="glyph"><i></i><i></i><i></i></span>
      {#if status === 'live'}live sync{:else if status === 'connecting'}connecting…{:else}{status}{/if}
    </span>
  </header>

  <div class="intro">
    <h1>Live filtered sync, in your browser.</h1>
    <p class="lede">
      This tab subscribes to the <code>tasks</code> table on the local dev server, applies every
      change through the real Rust→WASM engine, and shows the checkpoint advancing. Insert a row in
      Postgres — watch it arrive, filtered by your predicate. Reload the tab: it resumes from the
      checkpoint, no full replay.
    </p>
    <p class="ceiling">
      Rows are in-memory <a href="/blog/adr-0017" target="_blank" rel="noreferrer">(ADR-0017)</a>.
      Reload replays from the checkpoint — the one re-fetch is the v0.1 ceiling, not data loss.
    </p>
  </div>

  <form class="controls" onsubmit={(e) => e.preventDefault()}>
    <label class="grow">
      <span>Server URL</span>
      <input bind:value={url} placeholder="ws://localhost:8800/sync" spellcheck="false" />
    </label>
    <label>
      <span>Table</span>
      <input bind:value={table} spellcheck="false" />
    </label>
    <label class="grow">
      <span>where_sql <em>(safe-SQL subset; empty = all rows)</em></span>
      <input bind:value={whereSql} placeholder="completed = false" spellcheck="false" />
    </label>
    <label>
      <span>Token <em>(anonymous: leave blank)</em></span>
      <input bind:value={token} spellcheck="false" />
    </label>
    <div class="actions">
      {#if status === 'live' || status === 'connecting'}
        <Button variant="ghost" onclick={disconnect}>Disconnect</Button>
      {:else}
        <Button variant="mark" onclick={connect}>Connect &amp; subscribe</Button>
      {/if}
      <Button variant="ghost" onclick={reset}>Reset checkpoint</Button>
    </div>
    {#if err}<p class="err">{err}</p>{/if}
    <p class="hint">
      The dev <code>tasks</code> schema has columns
      <code>id, org_id, assignee_id, title, completed, created_at, updated_at</code>. Try
      <code>completed = false</code> or <code>title = 'ship'</code>. Grammar: six comparison ops +
      <code>AND</code>/<code>OR</code>/<code>NOT</code> + parens.
    </p>
  </form>

  <div class="stats">
    <div class="stat">
      <dt>Checkpoint LSN</dt>
      <dd class="tnum mono">{checkpoint}</dd>
      <span class="sub">{#if resumedFrom !== null}resumed from {resumedFrom}{/if}</span>
    </div>
    <div class="stat">
      <dt>Rows in store</dt>
      <dd class="tnum mono">{rowCount}</dd>
      <span class="sub">applied via the WASM engine</span>
    </div>
    <div class="stat">
      <dt>Matching rows</dt>
      <dd class="tnum mono">{rows.length}</dd>
      <span class="sub">current state of subscribed table</span>
    </div>
  </div>

  <div class="rows-wrap">
    <div class="rows-head">Live rows — current table state</div>
    {#if rows.length === 0}
      <p class="empty">
        No rows yet. Connect, then insert one from Postgres:<br />
        <code>docker compose -f docker/docker-compose.yml exec -T postgres psql -U cairn -d cairn \
          -c "INSERT INTO tasks (org_id, title) VALUES (gen_random_uuid(), 'ship v0.1')"</code>
      </p>
    {:else}
      <ul class="rows">
        {#each rows as r (r.pk)}
          <li>
            <span class="pk mono">{r.pk}</span>
            <span class="payload mono">
              {#if typeof r.payload === 'string'}{r.payload}{:else}{JSON.stringify(r.payload)}{/if}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  <div class="kill-note">
    <strong>Kill the server, reload, resume.</strong> Stop <code>make dev-stack</code> (Ctrl-C),
    insert a row in Postgres while the tab is disconnected, restart the server, and reload this tab.
    The checkpoint survives in <code>localStorage</code>; the client resubscribes with
    <code>resume_lsn</code> and picks up only what it missed (ADR-0009). The in-memory rows are
    rebuilt from the replay — that is the documented v0.1 ceiling.
  </div>
</section>

<style>
  .demo {
    max-width: 980px;
    margin: 0 auto;
    padding: 40px var(--gutter) 96px;
  }
  .demo-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 40px;
  }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: 9px;
    font-weight: 800;
    font-size: var(--t-20);
    letter-spacing: -0.02em;
  }
  .sync-pill {
    font-size: 13px;
    color: var(--ink-soft);
  }
  .sync-pill.live {
    color: var(--mark-ink);
  }
  .sync-pill .glyph {
    display: inline-flex;
    gap: 2px;
    margin-right: 6px;
    vertical-align: middle;
  }
  .sync-pill .glyph i {
    width: 3px;
    height: 11px;
    background: var(--stone-2);
    border-radius: 1px;
    display: inline-block;
  }
  .sync-pill.live .glyph i {
    background: var(--mark);
    animation: pulse 1.1s var(--ease) infinite;
  }
  .sync-pill.live .glyph i:nth-child(2) {
    animation-delay: 0.15s;
  }
  .sync-pill.live .glyph i:nth-child(3) {
    animation-delay: 0.3s;
  }
  @keyframes pulse {
    0%,
    100% {
      transform: scaleY(0.5);
      opacity: 0.6;
    }
    50% {
      transform: scaleY(1);
      opacity: 1;
    }
  }

  .intro h1 {
    font-size: 1.7rem;
    font-weight: 640;
    letter-spacing: -0.02em;
    margin-bottom: 10px;
  }
  .lede {
    color: var(--ink-soft);
    line-height: var(--lh-body);
    max-width: 64ch;
  }
  .ceiling {
    font-size: 13px;
    color: var(--stone-3);
    margin-top: 10px;
  }
  .ceiling a {
    color: var(--mark-ink);
  }

  .controls {
    margin-top: 28px;
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px 16px;
    padding: 24px;
    border: 1px solid var(--rule);
    border-radius: var(--radius-lg);
    background: var(--paper-2);
  }
  .controls label {
    display: grid;
    gap: 5px;
    font-size: 12px;
    color: var(--ink-soft);
  }
  .controls label em {
    font-style: normal;
    color: var(--stone-3);
  }
  .controls .grow {
    grid-column: 1 / -1;
  }
  .controls input {
    padding: 10px 12px;
    border: 1px solid var(--stone-2);
    border-radius: var(--radius);
    background: var(--paper);
    color: var(--ink);
    font-family: var(--font-mono);
    font-size: 13px;
  }
  .controls input:focus {
    outline: 2px solid var(--mark);
    outline-offset: 1px;
  }
  .actions {
    grid-column: 1 / -1;
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
  }
  .err {
    grid-column: 1 / -1;
    color: var(--mark-ink);
    font-size: 13px;
  }
  .hint {
    grid-column: 1 / -1;
    font-size: 12px;
    color: var(--stone-3);
    line-height: 1.5;
  }

  .stats {
    margin-top: 24px;
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 12px;
  }
  .stat {
    padding: 20px;
    border: 1px solid var(--rule);
    border-radius: var(--radius-lg);
    background: var(--paper-2);
  }
  .stat dt {
    font-size: 11px;
    color: var(--stone-3);
    text-transform: uppercase;
    letter-spacing: 0.1em;
  }
  .stat dd {
    font-size: 1.5rem;
    font-weight: 600;
    margin-top: 6px;
  }
  .stat .sub {
    display: block;
    font-size: 11px;
    color: var(--stone-3);
    margin-top: 4px;
  }
  .tnum {
    font-variant-numeric: tabular-nums;
  }
  .mono {
    font-family: var(--font-mono);
  }

  .rows-wrap {
    margin-top: 24px;
    border: 1px solid var(--rule);
    border-radius: var(--radius-lg);
    background: var(--code-bg);
    color: var(--code-fg);
    overflow: hidden;
  }
  .rows-head {
    padding: 10px 16px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--code-fg);
    opacity: 0.7;
    border-bottom: 1px solid color-mix(in srgb, var(--code-fg) 12%, transparent);
  }
  .empty {
    padding: 20px 16px;
    font-size: 13px;
    line-height: 1.6;
    opacity: 0.85;
  }
  .empty code {
    font-family: var(--font-mono);
    font-size: 12px;
    word-break: break-all;
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 340px;
    overflow-y: auto;
  }
  .rows li {
    display: grid;
    grid-template-columns: 200px 1fr;
    gap: 12px;
    padding: 8px 16px;
    font-size: 12px;
    border-bottom: 1px solid color-mix(in srgb, var(--code-fg) 6%, transparent);
    align-items: center;
  }
  .rows li:last-child {
    border-bottom: 0;
  }
  .rows .pk {
    opacity: 0.8;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rows .payload {
    opacity: 0.92;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .kill-note {
    margin-top: 24px;
    padding: 18px 20px;
    border: 1px solid color-mix(in srgb, var(--mark) 35%, transparent);
    border-radius: var(--radius-lg);
    background: color-mix(in srgb, var(--mark) 7%, var(--paper-2));
    font-size: 13px;
    line-height: 1.55;
    color: var(--ink-soft);
  }
  .kill-note strong {
    color: var(--ink);
  }
  .kill-note code {
    font-family: var(--font-mono);
    font-size: 12px;
  }

  @media (max-width: 720px) {
    .controls {
      grid-template-columns: 1fr;
    }
    .stats {
      grid-template-columns: 1fr;
    }
    .rows li {
      grid-template-columns: 90px 1fr;
    }
  }
</style>
