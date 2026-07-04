<script lang="ts">
  import { onMount } from 'svelte';
  import { Nostos, Button } from '$lib/components/ui';
  // Lazy/dynamic import: adapter-static prerenders the site, and the wasm
  // module must not be pulled into the top-level graph (it references
  // `import.meta.url` + fetches a .wasm at runtime). Loading it inside onMount
  // keeps the static export clean (gate 4). The default export is the async
  // `init()` (wasm-bindgen convention); named exports are the classes.
  import type { NostosEngine, Outcome } from 'nostos-ffi-wasm';

  /**
   * /demo — the moat, visible in a browser tab.
   *
   * One WebSocket to the dev-stack server (`make dev-stack`), a `where_sql`
   * predicate input, the live filtered rows, and the checkpoint LSN ticking
   * forward. Reload the tab → it resumes from the persisted checkpoint.
   *
   * Architecture (read this before editing): the page owns the WebSocket and
   * drives a real `NostosEngine` (from nostos-ffi-wasm) with every inbound frame.
   * The engine owns the in-memory row store + the checkpoint; the page reads
   * `engine.checkpoint` / `engine.rowCount` to render the durable state, and
   * renders the rows from the frames it decodes for display.
   *
   * Why not `NostosSocket.connect(...)`? That wrapper owns its own `onmessage`
   * and there is no API to read applied rows back out of the engine — so a
   * page that used `NostosSocket` could show counts + checkpoint but not the
   * rows themselves. This page keeps the engine (the load-bearing part: the
   * atomic apply, idempotency, checkpoint) and re-implements the ~30 lines of
   * WS glue `NostosSocket` provides. The glue it duplicates is the exact seam
   * ADR-0017 marks "WS glue untested in CI; covered by the E3 demo page manual
   * check" — this page IS that manual check, made interactive.
   * ponytail: when the engine gains a row-readback API (OPFS slice, E2
   * follow-up), swap this back to `NostosSocket` and drop the local decode.
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
  // the raw hex). The engine stores opaque bytes; we render what came off the
  // wire so the operator can see real data flowing.
  type Row = { pk: string; op: string; lsn: number; payload: Record<string, unknown> | string };
  let rows = $state<Row[]>([]);
  let checkpoint = $state(0);
  let rowCount = $state(0);
  let resumedFrom = $state<number | null>(null);

  let ws: WebSocket | null = null;
  let engine: NostosEngine | null = null;
  let initMod: typeof import('nostos-ffi-wasm') | null = null;

  const CHECKPOINT_KEY = () => `cairn:checkpoint:${table}`;

  /** Hex-decode the wire payload (the engine's transport does the same). */
  function decodeHex(hex: string): Uint8Array {
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < hex.length; i += 2) {
      bytes[i / 2] = parseInt(hex.slice(i, i + 2), 16);
    }
    return bytes;
  }

  /**
   * Decode a hex payload to a displayable value: the PgReplicator emits a small
   * JSON object {col:val} (see the extractor in crates/nostos-server/src/main.rs);
   * try that first, fall back to raw hex on any failure. The apply engine itself
   * never inspects payload contents — this is display-only.
   */
  function decodePayload(hex: string | null | undefined): Record<string, unknown> | string {
    if (!hex) return '(delete — no payload)';
    try {
      const text = new TextDecoder().decode(decodeHex(hex));
      return JSON.parse(text) as Record<string, unknown>;
    } catch {
      return hex;
    }
  }

  /** The persisted checkpoint drives `resume_lsn` on (re)connect. */
  function readCheckpoint(): number {
    const raw = localStorage.getItem(CHECKPOINT_KEY());
    const n = raw ? parseInt(raw, 10) : NaN;
    return Number.isFinite(n) ? n : 0;
  }
  function writeCheckpoint(lsn: number) {
    localStorage.setItem(CHECKPOINT_KEY(), String(lsn));
  }

  /**
   * Decode one inbound WS message into display rows + NostosEngine frames, apply,
   * flush, ack, persist. Mirrors `transport::on_message` in nostos-ffi-wasm: a
   * message may be a single object OR a JSON array of frames (C3 batched writes).
   */
  function handleMessage(data: ArrayBuffer | string) {
    if (!engine || !ws) return;
    const text = typeof data === 'string' ? data : new TextDecoder().decode(data);
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      return; // drop malformed (matches decode_frames' contract)
    }
    const frames: Record<string, unknown>[] = Array.isArray(parsed) ? parsed : [parsed];
    if (!frames.length) return;

    // Build + feed NostosEngine frames. The engine takes a Uint8Array payload;
    // we hex-decode once at the boundary (same as the transport's decode_hex).
    // We feed frames one at a time and flush at the end of the message — the
    // engine's soft-cap commit isn't needed here (we want every row visible).
    for (const f of frames) {
      const lsn = Number(f.lsn ?? 0);
      const op = String(f.op ?? 'insert');
      const pk = String(f.pk ?? '');
      const payloadHex = typeof f.payload === 'string' ? (f.payload as string) : null;
      const payloadBytes = payloadHex ? decodeHex(payloadHex) : null;

      // Feed the real WASM engine. The Frame constructor signature is
      // (lsn, op, table, pk, payload?, txn_id?).
      const { Frame } = initMod!;
      engine.feed(new Frame(lsn, op, table, pk, payloadBytes, null));

      // Render the row for display (the engine gives no readback API).
      rows = [{ pk, op, lsn, payload: decodePayload(payloadHex) }, ...rows].slice(0, 200);
    }

    // Flush → atomic commit → checkpoint advances. The Outcome carries the new
    // checkpoint + rows-applied; we ack it to the server + persist it.
    const outcome = engine.flush() as Outcome | undefined;
    if (outcome) {
      checkpoint = outcome.checkpoint;
      writeCheckpoint(outcome.checkpoint);
      ws.send(JSON.stringify({ type: 'ack', lsn: outcome.checkpoint }));
    }
    rowCount = engine.rowCount;
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
      // Fresh engine per session (in-memory rows do NOT survive reload —
      // ADR-0017. Only the checkpoint does, read below).
      engine = new initMod.NostosEngine();

      const resumeLsn = readCheckpoint();
      resumedFrom = resumeLsn;

      const fullUrl = token ? `${url}${url.includes('?') ? '&' : '?'}token=${token}` : url;
      ws = new WebSocket(fullUrl);
      ws.binaryType = 'arraybuffer';

      ws.onopen = () => {
        // Build + send the subscribe frame. `filters: []` is required on the
        // wire (the server rejects the frame without it); where_sql is omitted
        // when empty. resume_lsn lets the server skip already-acked events.
        const sub: Record<string, unknown> = { type: 'subscribe', table, filters: [] };
        if (resumeLsn > 0) sub.resume_lsn = resumeLsn;
        if (whereSql.trim()) sub.where_sql = whereSql.trim();
        ws!.send(JSON.stringify(sub));
        status = 'live';
      };
      ws.onmessage = (e) => handleMessage(e.data);
      ws.onerror = () => {
        status = 'error';
        err = 'WebSocket error — is the dev-stack server running? (make dev-stack)';
      };
      ws.onclose = () => {
        if (status !== 'error') status = 'closed';
      };
    } catch (e) {
      status = 'error';
      err = (e as Error).message;
    }
  }

  function disconnect() {
    ws?.close();
    ws = null;
    status = 'idle';
  }

  /** Wipe the persisted checkpoint + in-memory rows (start completely fresh). */
  function reset() {
    localStorage.removeItem(CHECKPOINT_KEY());
    rows = [];
    checkpoint = 0;
    rowCount = 0;
    resumedFrom = null;
  }

  onMount(() => () => disconnect());
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
      <dt>Rendered</dt>
      <dd class="tnum mono">{rows.length}</dd>
      <span class="sub">most recent first</span>
    </div>
  </div>

  <div class="rows-wrap">
    <div class="rows-head">Live rows</div>
    {#if rows.length === 0}
      <p class="empty">
        No rows yet. Connect, then insert one from Postgres:<br />
        <code>docker compose -f docker/docker-compose.yml exec -T postgres psql -U cairn -d cairn \
          -c "INSERT INTO tasks (org_id, title) VALUES (gen_random_uuid(), 'ship v0.1')"</code>
      </p>
    {:else}
      <ul class="rows">
        {#each rows as r (r.lsn + '-' + r.pk)}
          <li>
            <span class="lsn mono">{r.lsn}</span>
            <span class="op op-{r.op}">{r.op}</span>
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
  .demo { max-width: 980px; margin: 0 auto; padding: 40px var(--gutter) 96px; }
  .demo-head {
    display: flex; align-items: center; justify-content: space-between;
    margin-bottom: 40px;
  }
  .brand { display: inline-flex; align-items: center; gap: 9px; font-weight: 800; font-size: var(--t-20); letter-spacing: -0.02em; }
  .sync-pill { font-size: 13px; color: var(--ink-soft); }
  .sync-pill.live { color: var(--mark-ink); }
  .sync-pill .glyph { display: inline-flex; gap: 2px; margin-right: 6px; vertical-align: middle; }
  .sync-pill .glyph i { width: 3px; height: 11px; background: var(--stone-2); border-radius: 1px; display: inline-block; }
  .sync-pill.live .glyph i { background: var(--mark); animation: pulse 1.1s var(--ease) infinite; }
  .sync-pill.live .glyph i:nth-child(2) { animation-delay: 0.15s; }
  .sync-pill.live .glyph i:nth-child(3) { animation-delay: 0.3s; }
  @keyframes pulse { 0%, 100% { transform: scaleY(0.5); opacity: 0.6; } 50% { transform: scaleY(1); opacity: 1; } }

  .intro h1 { font-size: 1.7rem; font-weight: 640; letter-spacing: -0.02em; margin-bottom: 10px; }
  .lede { color: var(--ink-soft); line-height: var(--lh-body); max-width: 64ch; }
  .ceiling { font-size: 13px; color: var(--stone-3); margin-top: 10px; }
  .ceiling a { color: var(--mark-ink); }

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
  .controls label { display: grid; gap: 5px; font-size: 12px; color: var(--ink-soft); }
  .controls label em { font-style: normal; color: var(--stone-3); }
  .controls .grow { grid-column: 1 / -1; }
  .controls input {
    padding: 10px 12px;
    border: 1px solid var(--stone-2);
    border-radius: var(--radius);
    background: var(--paper);
    color: var(--ink);
    font-family: var(--font-mono);
    font-size: 13px;
  }
  .controls input:focus { outline: 2px solid var(--mark); outline-offset: 1px; }
  .actions { grid-column: 1 / -1; display: flex; gap: 10px; flex-wrap: wrap; }
  .err { grid-column: 1 / -1; color: var(--mark-ink); font-size: 13px; }
  .hint { grid-column: 1 / -1; font-size: 12px; color: var(--stone-3); line-height: 1.5; }

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
  .stat dt { font-size: 11px; color: var(--stone-3); text-transform: uppercase; letter-spacing: 0.1em; }
  .stat dd { font-size: 1.5rem; font-weight: 600; margin-top: 6px; }
  .stat .sub { display: block; font-size: 11px; color: var(--stone-3); margin-top: 4px; }
  .tnum { font-variant-numeric: tabular-nums; }
  .mono { font-family: var(--font-mono); }

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
  .empty { padding: 20px 16px; font-size: 13px; line-height: 1.6; opacity: 0.85; }
  .empty code { font-family: var(--font-mono); font-size: 12px; word-break: break-all; }
  .rows { list-style: none; margin: 0; padding: 0; max-height: 340px; overflow-y: auto; }
  .rows li {
    display: grid;
    grid-template-columns: 90px 70px 130px 1fr;
    gap: 12px;
    padding: 8px 16px;
    font-size: 12px;
    border-bottom: 1px solid color-mix(in srgb, var(--code-fg) 6%, transparent);
    align-items: center;
  }
  .rows li:last-child { border-bottom: 0; }
  .rows .lsn { opacity: 0.6; text-align: right; }
  .rows .op { font-weight: 600; }
  .rows .op-insert { color: #6fcf97; }
  .rows .op-update { color: #f2c94c; }
  .rows .op-delete { color: #eb5757; }
  .rows .pk { opacity: 0.8; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .rows .payload { opacity: 0.92; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

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
  .kill-note strong { color: var(--ink); }
  .kill-note code { font-family: var(--font-mono); font-size: 12px; }

  @media (max-width: 720px) {
    .controls { grid-template-columns: 1fr; }
    .stats { grid-template-columns: 1fr; }
    .rows li { grid-template-columns: 60px 60px 1fr; }
    .rows .pk { display: none; }
  }
</style>
