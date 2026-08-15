// nostos.sw.js — EXPERIMENTAL Service Worker for Web Push (plan task 6.2,
// ADR-0037 §6 Wave 3). Default OFF: nothing registers this file unless the
// host app explicitly opts in via push.js `enableWebPush` (ADR-0033
// experimental-flag discipline — no behavior change for existing embedders).
//
// Role: a WAKE RELAY, not an engine host. The wasm apply engine stays in the
// module Worker (worker/nostos.worker.js — it owns the socket + storage); this
// SW only:
//
//   push               → postMessage {type:"nostos:wake", table, lsn} to every
//                        open window client (the page forwards it to the
//                        module Worker, which reconnects + resumes from the
//                        durable checkpoint — sync is the transport, the push
//                        is just the doorbell, ADR-0037 §2), and show a
//                        notification when no client is open (the visible
//                        nudge that brings the user back).
//   notificationclick  → focus an existing client, else open one.
//   message            → {type:"nostos:simulate-push", payload} drives the
//                        SAME handlePush as a real `push` event (the headless
//                        harness cannot mint encrypted pushes through a real
//                        push service), plus a ping probe for registration.
//
// ponytail ceiling: with NO open client (app killed), the push arrives but
// wakes nothing — the engine does not run inside the SW (importing the wasm
// bundle here needs the full 6.1 storage seam on the SW's postMessage
// transport; deliberately not built until this experiment proves out). The
// visible notification + the durable LSN checkpoint are the correctness
// story: data is never lost, only the immediate wake is. Upgrade path: host
// the engine in the SW (KV seam is already injectable — setKvStore) or adopt
// the Push API's declarative-push draft if it ships.

"use strict";

const WAKE_TYPE = "nostos:wake";

// The push handler. Payload is the doorbell shape (ADR-0037 §2): at most
// {table, lsn} for silent wakes, plus optional {title, body} when the server
// routed a visible template. Never row data.
async function handlePush(payload) {
  const data = payload || {};
  const clients = await self.clients.matchAll({
    type: "window",
    includeUncontrolled: true,
  });

  // Wake every open client: the page's push.js listener forwards to the
  // module Worker (its `wake` command reconnects if the socket is down; an
  // open socket needs nothing — the WS stream IS the transport).
  for (const client of clients) {
    try {
      client.postMessage({
        type: WAKE_TYPE,
        table: data.table ?? null,
        lsn: data.lsn ?? null,
      });
    } catch (_) {
      /* client tore down between matchAll and postMessage — ignore */
    }
  }

  // Visible half. userVisibleOnly:true subscriptions expect a notification
  // per push (Chromium penalizes silent handling), so show one whenever no
  // client is open — the app is closed and this nudge is what re-opens it.
  // `tag: "nostos"` collapses to newest-wins (the in-rail Topic semantic,
  // RFC 8030 §5.4, mirrored client-side).
  if (clients.length === 0) {
    await self.registration.showNotification(data.title || "Nostos", {
      body: data.body || "New data available",
      tag: "nostos",
      renotify: true,
    });
  }
}

self.addEventListener("push", (event) => {
  let payload = null;
  try {
    payload = event.data ? event.data.json() : null;
  } catch (_) {
    /* non-JSON payload — treat as a bare doorbell (payload null) */
  }
  event.waitUntil(handlePush(payload));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(
    (async () => {
      const clients = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      for (const client of clients) {
        return client.focus(); // focus the first open window
      }
      return self.clients.openWindow("/");
    })(),
  );
});

self.addEventListener("message", (event) => {
  const m = event.data || {};
  if (m.type === "nostos:simulate-push") {
    // Test hook: drives the SAME handler a real `push` event uses.
    void handlePush(m.payload);
    return;
  }
  if (m.type === "nostos:ping" && event.source) {
    event.source.postMessage({ type: "nostos:pong" });
  }
});
