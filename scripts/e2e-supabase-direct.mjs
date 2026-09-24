#!/usr/bin/env node
// Direct-mode end-to-end against a REAL Supabase stack — PostgREST, Realtime,
// GoTrue and the Edge Runtime, not stubs.
//
//   cd <a supabase project dir> && supabase start
//   node scripts/e2e-supabase-direct.mjs
//
// ## Why this exists
//
// `crates/nostos-cli/tests/e2e_pg_direct_sql.rs` proves the generated SQL
// against plain Postgres, with `auth`, `realtime` and `net` stubbed. Everything
// Supabase-specific is therefore unproven there, and it is exactly the part a
// developer cannot debug from the client side:
//
//   - how PostgREST renders `xid8` (the horizon is a cursor; if it arrives as a
//     JS number it silently loses precision past 2^53),
//   - whether `raise sqlstate 'PT410'` really surfaces as HTTP 410,
//   - whether `auth.jwt()` inside `nostos.current_scopes()` sees the same claims
//     under real GoTrue tokens,
//   - whether a private Realtime channel authorized by the generated policy on
//     `realtime.messages` actually delivers — and refuses the wrong tenant,
//   - whether `pg_net` can reach an Edge Function and whether that function can
//     read the token registry at all.
//
// Every check below is one of those. Configuration comes from the environment
// so the same script runs against a hosted project (`NOSTOS_SB_URL=https://…`),
// where `NOSTOS_SB_PSQL` is the one thing that has to change.

import { execFileSync } from "node:child_process";

const URL_BASE = process.env.NOSTOS_SB_URL ?? "http://127.0.0.1:54321";
const ANON = process.env.NOSTOS_SB_ANON_KEY;
const SERVICE = process.env.NOSTOS_SB_SERVICE_KEY;
// The generated objects live in the `nostos` schema, which is deliberately NOT
// exposed to the API — so the fixture steps that touch it need SQL. Default is
// the local stack's container; override for a hosted project.
const PSQL = (process.env.NOSTOS_SB_PSQL ??
  "docker exec -i supabase_db_stack psql -qtAX -v ON_ERROR_STOP=1 -U postgres -d postgres")
  .split(" ");
// What pg_net (inside the database container) must dial to reach the function.
// Not the same host the test client uses: `127.0.0.1` there is the database.
const PUSH_ENDPOINT = process.env.NOSTOS_SB_PUSH_ENDPOINT ??
  "http://host.docker.internal:54321/functions/v1/nostos-push";

if (!ANON || !SERVICE) {
  console.error("set NOSTOS_SB_ANON_KEY and NOSTOS_SB_SERVICE_KEY (supabase status -o json)");
  process.exit(2);
}

const sql = (text) =>
  execFileSync(PSQL[0], [...PSQL.slice(1)], { input: text, encoding: "utf8" }).trim();

let failures = 0;
const results = [];
function check(label, ok, detail = "") {
  results.push(`${ok ? "✓" : "✗"} ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
}
// Mirrors `direct::Verdict::Note`: something the operator needs to know that
// no assertion can settle — a dashboard switch, a platform default.
function note(label, detail = "") {
  results.push(`• ${label}${detail ? ` — ${detail}` : ""}`);
}
// Every mutation the run depends on. A silent 4xx here used to surface three
// checks later as "the log is empty", which is the wrong bug to go looking for.
function must(label, res, want = 201) {
  if (res.status !== want) {
    throw new Error(`${label}: HTTP ${res.status} ${JSON.stringify(res.body).slice(0, 160)}`);
  }
  return res.body;
}
async function section(label, body) {
  try {
    await body();
  } catch (e) {
    check(label, false, `threw: ${e.message}`);
  }
}

// --- transport -------------------------------------------------------------

async function rest(path, { token, method = "POST", body, prefer } = {}) {
  const headers = {
    apikey: ANON,
    Authorization: `Bearer ${token ?? ANON}`,
    "Content-Type": "application/json",
  };
  if (prefer) headers.Prefer = prefer;
  const res = await fetch(`${URL_BASE}/rest/v1${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = text;
  }
  return { status: res.status, body: json };
}

const pull = (token, since = "0", maxTxns = 200) =>
  rest("/rpc/nostos_pull", { token, body: { since, max_txns: maxTxns } });

// Pull until `want` is satisfied, or give up. This is not test flake padding —
// it is the protocol. The horizon is `pg_snapshot_xmin`, so a just-committed
// row stays invisible while ANY older transaction is still open, and on a
// Supabase stack pg_net's queue worker opens one on a timer. Normally that
// window is microseconds; a device that pulls inside it simply pulls again.
async function pullUntil(token, want, { timeoutMs = 10000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let last = await pull(token);
  while (Date.now() < deadline) {
    if (Array.isArray(last.body) && want(last.body)) return last;
    await new Promise((r) => setTimeout(r, 250));
    last = await pull(token);
  }
  return last;
}

async function signUp(email) {
  const res = await fetch(`${URL_BASE}/auth/v1/signup`, {
    method: "POST",
    headers: { apikey: ANON, "Content-Type": "application/json" },
    body: JSON.stringify({ email, password: "nostos-e2e-password" }),
  });
  const j = await res.json();
  const token = j.access_token;
  if (!token) throw new Error(`signup failed: ${JSON.stringify(j)}`);
  const sub = JSON.parse(Buffer.from(token.split(".")[1], "base64url")).sub;
  return { token, sub };
}

// --- the Realtime doorbell -------------------------------------------------

// Join one private channel and resolve what happened: `{ joined, rings }`.
// `after` runs once the join is acknowledged, so a ring it triggers cannot be
// missed by racing the subscription; the wait then ends as soon as the first
// ring lands rather than burning the whole budget.
//
// `waitMs` is deliberately generous. The FIRST join against a cold Realtime
// tenant pays connection setup before any policy is even evaluated, which is
// long enough to look exactly like "the doorbell does not work".
function doorbell(topic, token, { private: isPrivate = true, after, waitMs = 15000 } = {}) {
  return new Promise((resolve) => {
    const ws = new WebSocket(
      `${URL_BASE.replace(/^http/, "ws")}/realtime/v1/websocket?apikey=${ANON}&vsn=1.0.0`,
    );
    const out = { joined: null, reason: null, rings: 0 };
    let timer;
    const done = () => {
      clearTimeout(timer);
      try {
        ws.close();
      } catch {
        /* already closed */
      }
      resolve(out);
    };
    ws.onopen = () => {
      ws.send(JSON.stringify({
        topic: `realtime:${topic}`,
        event: "phx_join",
        ref: "1",
        join_ref: "1",
        payload: {
          config: {
            broadcast: { ack: false, self: false },
            presence: { enabled: false },
            private: isPrivate,
          },
          access_token: token,
        },
      }));
      timer = setTimeout(done, waitMs);
    };
    ws.onmessage = async (ev) => {
      const f = JSON.parse(ev.data);
      if (f.event === "phx_reply" && f.ref === "1") {
        out.joined = f.payload?.status === "ok";
        out.reason = f.payload?.response?.reason ?? null;
        if (!out.joined) return done();
        if (after) await after();
      }
      if (f.event === "broadcast" && f.payload?.event === "nostos_ring") {
        out.rings += 1;
        if (after) done();
      }
    };
    ws.onerror = () => {
      out.joined = out.joined ?? false;
      done();
    };
    // Realtime refuses an unauthorized join by closing the socket rather than
    // always replying, so a close before any reply IS the refusal.
    ws.onclose = () => {
      out.joined = out.joined ?? false;
      done();
    };
  });
}

// --- the run ---------------------------------------------------------------

const alice = await signUp(`alice-${Date.now()}@nostos.test`);
const bob = await signUp(`bob-${Date.now()}@nostos.test`);

// A clean log, and a retention row that cannot refuse anything yet.
sql("truncate nostos.changes; update nostos.retention set pruned_below = '0'::xid8 where id = 1;");

await section("pull", async () => {
  const { status, body } = await pull(alice.token);
  check("nostos_pull answers an authenticated device", status === 200, `HTTP ${status}`);
  check(
    "the anon key alone cannot pull",
    (await pull(null)).status >= 400,
    `HTTP ${(await pull(null)).status}`,
  );
  void body;
});

await section("xid8 over the wire", async () => {
  const r = await rest("/nostos_e2e_notes", {
    token: alice.token,
    body: { owner_id: alice.sub, body: "first" },
    prefer: "return=representation",
  });
  check("an authenticated insert is accepted", r.status === 201, `HTTP ${r.status}`);
  must("insert", r);
  const { body } = await pullUntil(alice.token, (rows) => rows.length > 0);
  const first = Array.isArray(body) ? body[0] : null;
  check("the insert reached the change log", !!first, JSON.stringify(body).slice(0, 120));
  // The load-bearing one: a horizon that arrives as a JS number is a cursor
  // that silently rounds past 2^53. PostgREST must emit xid8 as a string.
  check(
    "PostgREST renders xid8 as a JSON string, not a number",
    typeof first?.horizon === "string" && typeof first?.xid === "string",
    `horizon=${typeof first?.horizon} xid=${typeof first?.xid}`,
  );
  check("the row image travels as jsonb", first?.row?.body === "first");
});

await section("rls", async () => {
  const mine = await pull(alice.token);
  const theirs = await pull(bob.token);
  check(
    "RLS scopes the log to the caller's own claims",
    mine.body.length > 0 && theirs.body.length === 0,
    `alice=${mine.body.length} bob=${theirs.body.length}`,
  );

  // A public table is the other half of the scope story: one shared row every
  // authenticated device must see.
  must(
    "service-role catalog insert",
    await rest("/nostos_e2e_catalog", {
      token: SERVICE,
      body: { label: "shared" },
      prefer: "return=representation",
    }),
  );
  const after = await pullUntil(bob.token, (rows) =>
    rows.some((r) => r.table_name === "nostos_e2e_catalog"));
  check(
    "an unscoped table reaches every authenticated device",
    Array.isArray(after.body) && after.body.some((r) => r.table_name === "nostos_e2e_catalog"),
    `HTTP ${after.status} ${JSON.stringify(after.body).slice(0, 160)}`,
  );
});

await section("transactions", async () => {
  const before = Number(sql("select coalesce(max(seq), 0) from nostos.changes;"));
  // One statement, two rows: one transaction, so one xid. The client applies a
  // whole xid or none of it, which is the entire point of logging xid at all.
  must(
    "two-row insert",
    await rest("/nostos_e2e_notes", {
      token: alice.token,
      body: [
        { owner_id: alice.sub, body: "batch-a" },
        { owner_id: alice.sub, body: "batch-b" },
      ],
    }),
  );
  const rows = (await pullUntil(alice.token, (all) =>
    all.filter((r) => r.seq > before).length === 2)).body.filter((r) => r.seq > before);
  check(
    "rows written in one transaction share one xid",
    rows.length === 2 && rows[0].xid === rows[1].xid,
    `${rows.length} row(s), ${new Set(rows.map((r) => r.xid)).size} xid(s)`,
  );
});

await section("increment", async () => {
  const id = must(
    "counter insert",
    await rest("/nostos_e2e_notes", {
      token: alice.token,
      body: { owner_id: alice.sub, body: "counter" },
      prefer: "return=representation",
    }),
  )[0].id;
  for (let i = 0; i < 3; i += 1) {
    const r = await rest("/rpc/nostos_increment", {
      token: alice.token,
      body: { p_table: "nostos_e2e_notes", p_pk: id, p_field: "hits", p_delta: 1 },
    });
    if (r.status >= 300) throw new Error(`increment ${i}: HTTP ${r.status} ${JSON.stringify(r.body)}`);
  }
  const row = (await rest(`/nostos_e2e_notes?id=eq.${id}&select=hits`, {
    token: alice.token,
    method: "GET",
  })).body[0];
  check("three increments land as three", row.hits === 3, `hits=${row.hits}`);

  const refused = await rest("/rpc/nostos_increment", {
    token: alice.token,
    body: { p_table: "auth.users", p_pk: id, p_field: "hits", p_delta: 1 },
  });
  check(
    "increment refuses a table that is not synced",
    refused.status === 403 || refused.status === 401,
    `HTTP ${refused.status}`,
  );

  const notMine = await rest("/rpc/nostos_increment", {
    token: bob.token,
    body: { p_table: "nostos_e2e_notes", p_pk: id, p_field: "hits", p_delta: 100 },
  });
  const still = (await rest(`/nostos_e2e_notes?id=eq.${id}&select=hits`, {
    token: alice.token,
    method: "GET",
  })).body[0];
  check(
    "increment is authorized by RLS, not by its arguments",
    still.hits === 3,
    `HTTP ${notMine.status}, hits=${still.hits}`,
  );
});

await section("scope change", async () => {
  const before = Number(sql("select coalesce(max(seq), 0) from nostos.changes;"));
  const id = must(
    "moving-row insert",
    await rest("/nostos_e2e_notes", {
      token: alice.token,
      body: { owner_id: alice.sub, body: "moving" },
      prefer: "return=representation",
    }),
  )[0].id;
  // Alice hands the row to Bob. Alice must be TOLD it is gone; a row she can
  // no longer see can never be sent to her again.
  sql(`update public.nostos_e2e_notes set owner_id = '${bob.sub}' where id = '${id}';`);
  const mine = (await pullUntil(alice.token, (all) =>
    all.some((r) => r.seq > before && r.op === "delete"))).body.filter((r) => r.seq > before);
  check(
    "a row that changes scope is logged as a delete under the old scope",
    mine.some((r) => r.pk === id && r.op === "delete"),
    mine.map((r) => r.op).join(","),
  );
  const theirs = (await pullUntil(bob.token, (all) =>
    all.some((r) => r.pk === id && r.op === "update"))).body;
  check(
    "and as an update under the new one",
    theirs.some((r) => r.pk === id && r.op === "update"),
    theirs.map((r) => r.op).join(","),
  );
});

await section("retention", async () => {
  const head = sql("select pg_snapshot_xmin(pg_current_snapshot())::text;");
  sql(`update nostos.retention set pruned_below = '${head}'::xid8 where id = 1;`);
  const { status, body } = await pull(alice.token, "3");
  // A short page is indistinguishable from "nothing happened", so the window
  // has to be an error. PostgREST maps a PTxyz sqlstate to HTTP xyz.
  check(
    "a horizon below the pruned window is HTTP 410, not a short page",
    status === 410,
    `HTTP ${status} ${JSON.stringify(body).slice(0, 90)}`,
  );
  // A 410 the device cannot act on is a device bricked by a long holiday, so
  // the recovery path is part of the retention story, not a follow-up.
  const snap = await rest("/rpc/nostos_snapshot", { token: alice.token, body: {} });
  check(
    "a pruned device can still re-snapshot",
    snap.status === 200 && Array.isArray(snap.body) && snap.body.length > 0,
    `HTTP ${snap.status}`,
  );
  const horizons = new Set((snap.body ?? []).map((r) => r.horizon));
  check(
    "the whole snapshot comes from ONE cross-table view",
    horizons.size === 1,
    `${horizons.size} distinct horizon(s)`,
  );
  check(
    "it announces every synced table, empty ones included",
    ["nostos_e2e_notes", "nostos_e2e_catalog"].every((tbl) =>
      snap.body.some((r) => r.table_name === tbl && r.pk === null)),
    (snap.body ?? [])
      .filter((r) => r.pk === null && r.table_name)
      .map((r) => r.table_name)
      .join(","),
  );
  check(
    "and carries only rows RLS lets this device see",
    snap.body
      .filter((r) => r.table_name === "nostos_e2e_notes" && r.row)
      .every((r) => r.row.owner_id === alice.sub),
    `${snap.body.filter((r) => r.row).length} row(s)`,
  );
  const otherSnap = await rest("/rpc/nostos_snapshot", { token: null, body: {} });
  check(
    "the anon key cannot snapshot",
    otherSnap.status >= 400,
    `HTTP ${otherSnap.status}`,
  );
  sql("update nostos.retention set pruned_below = '0'::xid8 where id = 1;");
  const resumed = await pull(alice.token, snap.body[0].horizon);
  check(
    "and the snapshot's horizon is a valid resume point",
    resumed.status === 200,
    `HTTP ${resumed.status}`,
  );
});

await section("doorbell", async () => {
  // Warm the tenant up: the first join pays Realtime's connection setup, and
  // charging that to the first assertion is how a working doorbell reads as
  // broken.
  await doorbell(`nostos:sub:${alice.sub}`, alice.token, { waitMs: 8000 });

  const mine = await doorbell(`nostos:sub:${alice.sub}`, alice.token, {
    after: () =>
      rest("/nostos_e2e_notes", {
        token: alice.token,
        body: { owner_id: alice.sub, body: "ring" },
      }),
  });
  check("a device joins its own private channel", mine.joined === true, mine.reason ?? "");
  check("and the write rings it", mine.rings > 0, `${mine.rings} ring(s)`);

  const theirs = await doorbell(`nostos:sub:${alice.sub}`, bob.token, { waitMs: 8000 });
  check(
    "another tenant cannot join that channel",
    theirs.joined === false,
    theirs.reason ?? `joined=${theirs.joined}`,
  );

  // The reason `nostos doctor --mode direct` reports the dashboard toggle it
  // cannot check: the policy only governs PRIVATE channels. With public access
  // left on, the same wrong tenant joins the same topic by simply not asking
  // for a private one, and no policy is ever consulted. A local stack ships
  // with it on, so this is a note there and a check against a hosted project.
  const loophole = await doorbell(`nostos:sub:${alice.sub}`, bob.token, {
    private: false,
    waitMs: 8000,
  });
  if (loophole.joined) {
    note(
      "public access is ON — a non-private join reaches this topic with no policy check",
      'turn off "Allow public access" in the project\'s Realtime settings',
    );
  } else {
    check("public access is off, so the non-private loophole is closed too", true);
  }
});

await section("push", async () => {
  sql(
    `update nostos.push_config set endpoint = '${PUSH_ENDPOINT}', secret = 'e2e-secret' where id = 1;`,
  );
  const reg = await rest("/rpc/nostos_register_push_token", {
    token: alice.token,
    body: { p_platform: "fcm", p_token: "e2e-device-token" },
  });
  check("a device registers its push token", reg.status < 300, `HTTP ${reg.status}`);
  const stamped = sql(
    `select scope from nostos.push_tokens where token = 'e2e-device-token' order by scope;`,
  ).split("\n");
  check(
    "the token is stamped with the caller's own scope, never an argument",
    stamped.includes(`sub:${alice.sub}`),
    stamped.join(","),
  );

  // Counting `net._http_response` alone races pg_net's own worker: a request
  // sent earlier in the run lands mid-measurement and reads as a new push.
  // Clearing BOTH the queue and the responses makes each observation exact.
  const resetNet = () =>
    sql("delete from net.http_request_queue; delete from net._http_response;");
  const sent = () =>
    Number(
      sql("select (select count(*) from net.http_request_queue) + " +
        "(select count(*) from net._http_response);"),
    );

  // Nobody is awake, so the change must reach for the doorbell of last resort.
  resetNet();
  sql("delete from nostos.device_presence; delete from nostos.push_cooldown;");
  must(
    "asleep-scope insert",
    await rest("/nostos_e2e_notes", {
      token: alice.token,
      body: { owner_id: alice.sub, body: "asleep" },
    }),
  );
  let posted = "";
  for (let i = 0; i < 30 && !posted; i += 1) {
    await new Promise((r) => setTimeout(r, 500));
    posted = sql(
      "select status_code::text || ' ' || coalesce(left(content, 80), '') " +
        "from net._http_response where status_code is not null order by id desc limit 1;",
    );
  }
  check("pg_net reached the Edge Function", posted !== "", posted || "no response recorded");
  check(
    "the function accepted the shared secret",
    posted !== "" && !posted.startsWith("403"),
    posted.slice(0, 90),
  );
  // The check this whole section exists for. `nostos` is not an exposed schema,
  // so a function that selects `nostos.push_tokens` through the Data API gets
  // `Invalid schema: nostos` and drops every notification with no error anywhere
  // the operator will look. It has to go through `public.nostos_push_targets`.
  check(
    "the wake path is not broken by the unexposed nostos schema",
    posted !== "" && !/invalid schema|PGRST106|not exposed/i.test(posted),
    posted.slice(0, 90),
  );
  // Asserted directly rather than inferred from the function's status code: a
  // 500 from any later step (minting an FCM token, say) would otherwise read
  // as a healthy registry.
  const targets = await rest("/rpc/nostos_push_targets", {
    token: SERVICE,
    body: { p_scope: `sub:${alice.sub}` },
  });
  check(
    "the Edge Function's registry RPC returns this scope's tokens",
    targets.status === 200 && targets.body?.some?.((t) => t.token === "e2e-device-token"),
    `HTTP ${targets.status} ${JSON.stringify(targets.body).slice(0, 90)}`,
  );
  // These are `security definer`, so an EXECUTE grant left on a Data API role
  // bypasses every table grant and policy underneath. Supabase's DEFAULT
  // PRIVILEGES hand `anon` and `authenticated` EXECUTE on new public functions
  // by name, and `revoke … from public` does not take that away.
  for (const token of [alice.token, null]) {
    const who = token ? "an authenticated device" : "the anon key";
    const leaked = await rest("/rpc/nostos_push_targets", {
      token,
      body: { p_scope: `sub:${alice.sub}` },
    });
    check(
      `${who} cannot read the token registry`,
      leaked.status >= 400,
      `HTTP ${leaked.status} ${JSON.stringify(leaked.body).slice(0, 80)}`,
    );
  }
  const anonRegister = await rest("/rpc/nostos_register_push_token", {
    token: null,
    body: { p_platform: "fcm", p_token: "anon-smuggled" },
  });
  check(
    "the anon key cannot register a push token under the public scope",
    anonRegister.status >= 400,
    `HTTP ${anonRegister.status}`,
  );
  note(
    "FCM delivery itself is NOT exercised — no service-account credentials",
    "everything up to the `messages:send` call is",
  );

  // An awake device needs no push at all: the Realtime ring already reached it.
  resetNet();
  must(
    "heartbeat",
    await rest("/rpc/nostos_heartbeat", { token: alice.token, body: { p_device_id: "e2e-dev" } }),
    204,
  );
  must(
    "awake-scope insert",
    await rest("/nostos_e2e_notes", {
      token: alice.token,
      body: { owner_id: alice.sub, body: "awake" },
    }),
  );
  await new Promise((r) => setTimeout(r, 2500));
  check("an awake device is not pushed to", sent() === 0, `${sent()} request(s)`);

  // And the debounce: five writes into one sleeping scope are one request, not
  // five. This is also the answer to "does pg_net get hammered".
  resetNet();
  sql("delete from nostos.device_presence; delete from nostos.push_cooldown;");
  for (let i = 0; i < 5; i += 1) {
    must(
      `burst insert ${i}`,
      await rest("/nostos_e2e_notes", {
        token: alice.token,
        body: { owner_id: alice.sub, body: `burst-${i}` },
      }),
    );
  }
  await new Promise((r) => setTimeout(r, 2500));
  check("five writes to one sleeping scope debounce to one push", sent() === 1, `${sent()} request(s)`);
});

console.log(results.join("\n"));
console.log(failures === 0 ? "\nall checks passed" : `\n${failures} check(s) FAILED`);
process.exit(failures === 0 ? 0 : 1);
