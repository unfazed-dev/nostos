// push.js — EXPERIMENTAL Web Push client for @nostos-sync/web (plan task 6.2,
// ADR-0037 §6 Wave 3).
//
// FLAG: this module is default-OFF. Nothing in the SDK registers the Service
// Worker, asks for notification permission, or touches /push-tokens until the
// host app calls `enableWebPush(...)` with explicit VAPID + REST config
// (ADR-0033 experimental-flag discipline). Existing embedders that never call
// it see zero behavior change.
//
// What it does when enabled (all main-thread — pushManager.subscribe needs a
// window):
//   1. registers sw/nostos.sw.js (the wake relay),
//   2. requests Notification permission (the UX gate),
//   3. pushManager.subscribe({userVisibleOnly, applicationServerKey}) with the
//      host's VAPID PUBLIC key,
//   4. registers the subscription via the pinned REST contract:
//      POST {httpBase}/push-tokens  {"platform":"webpush","token":<subscription JSON>}
//      Authorization: Bearer <jwt>  → 204   (the whole subscription JSON is
//      the token — the server stores it as the Web Push endpoint + keys),
//   5. listens for the SW's {type:"nostos:wake"} client messages and calls
//      `onWake({table, lsn})` — the host forwards that to the module Worker
//      (worker `wake` command: reconnect if down, resume from checkpoint).
//
// `disable()` (call from the sign-out hook, ADR-0037 §3): unsubscribes +
// DELETE /push-tokens/{token} so the next principal receives nothing.
//
// ponytail ceilings:
//  - A killed app (no open client) gets the visible notification but no data
//    wake — the engine does not run in the SW yet (see sw/nostos.sw.js header).
//  - pushsubscriptionchange (server key rotation / browser renewal) is NOT
//    re-subscribed here; the host re-runs enableWebPush on login/boot, which
//    reuses-or-replaces the subscription. Add an SW-side resubscribe if the
//    experiment graduates.

"use strict";

const WAKE_TYPE = "nostos:wake";

/**
 * VAPID applicationServerKey conversion (RFC 8292): base64url-unpadded
 * (87-char) P-256 public point → the Uint8Array PushManager.subscribe wants.
 * Exported for tests.
 * @param {string} b64
 * @returns {Uint8Array}
 */
export function urlB64ToUint8Array(b64) {
  const padding = "=".repeat((4 - (b64.length % 4)) % 4);
  const b64u = (b64 + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(b64u);
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

/**
 * Register a push subscription with the server — the PINNED contract
 * (crates/nostos-server/src/push_api.rs): POST {httpBase}/push-tokens,
 * Content-Type application/json, Authorization Bearer, body
 * {"platform":"webpush","token":JSON.stringify(subscription)} → 204.
 * Resolves on 204; rejects with status + body otherwise. Exported for tests
 * and for hosts that subscribe themselves (e.g. an existing registration).
 * @param {PushSubscription} sub
 * @param {{httpBase: string, token?: string|null}} cfg
 * @returns {Promise<void>}
 */
export async function registerSubscription(sub, cfg) {
  const resp = await fetch(cfg.httpBase + "/push-tokens", {
    method: "POST",
    headers: Object.assign(
      { "Content-Type": "application/json" },
      cfg.token ? { Authorization: "Bearer " + cfg.token } : {},
    ),
    body: JSON.stringify({
      platform: "webpush",
      token: JSON.stringify(sub),
    }),
  });
  if (resp.status !== 204) {
    throw new Error(
      "nostos push: register failed: HTTP " + resp.status + " " + (await resp.text()),
    );
  }
}

/**
 * Deregister — DELETE /push-tokens/{token} (owner-scoped, 204 on success or
 * no-op). The token is encodeURIComponent'd: the webpush token is a full
 * subscription JSON whose endpoint URL contains "/" — raw interpolation would
 * split the path.
 * @param {PushSubscription} sub
 * @param {{httpBase: string, token?: string|null}} cfg
 * @returns {Promise<void>}
 */
export async function deregisterSubscription(sub, cfg) {
  const resp = await fetch(
    cfg.httpBase + "/push-tokens/" + encodeURIComponent(JSON.stringify(sub)),
    {
      method: "DELETE",
      headers: Object.assign(
        {},
        cfg.token ? { Authorization: "Bearer " + cfg.token } : {},
      ),
    },
  );
  if (resp.status !== 204) {
    throw new Error("nostos push: deregister failed: HTTP " + resp.status);
  }
}

/**
 * EXPERIMENTAL (flag) — enable Web Push for this page. See the module header.
 *
 * @param {{
 *   vapidPublicKey: string,      // base64url P-256 public half of the server's NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY keypair
 *   httpBase: string,            // REST base, e.g. "http://host:8080" (the http(s) form of the sync ws:// url)
 *   token?: string|null,         // the same JWT the sync connection uses (Bearer on /push-tokens)
 *   swUrl?: string,              // default "/sw/nostos.sw.js" — serve it at the scope root (or send Service-Worker-Allowed)
 *   scope?: string,              // default "/"
 *   onWake?: (detail:{table:string|null, lsn:number|null})=>void, // default: dispatches a "nostos:wake" CustomEvent on window
 * }} opts
 * @returns {Promise<{enabled: boolean, reason?: string, subscription: PushSubscription|null, disable: ()=>Promise<void>}>}
 */
export async function enableWebPush(opts) {
  if (!opts || !opts.vapidPublicKey || !opts.httpBase) {
    throw new Error("nostos push: enableWebPush requires {vapidPublicKey, httpBase}");
  }
  const swUrl = opts.swUrl || "/sw/nostos.sw.js";
  const scope = opts.scope || "/";
  const cfg = { httpBase: opts.httpBase, token: opts.token ?? null };

  // 1. Register the SW first — the wake relay works even if permission or
  //    subscribe fails (the experiment degrades to wake-only, never throws).
  const registration = await navigator.serviceWorker.register(swUrl, { scope });

  // 5 (wired early): SW → page wake messages. From SW client.postMessage,
  //    these arrive on navigator.serviceWorker regardless of control state.
  const onWake = opts.onWake || null;
  const wakeListener = (event) => {
    const d = event.data || {};
    if (d.type !== WAKE_TYPE) return;
    if (onWake) {
      onWake({ table: d.table ?? null, lsn: d.lsn ?? null });
    } else {
      window.dispatchEvent(new CustomEvent(WAKE_TYPE, { detail: d }));
    }
  };
  navigator.serviceWorker.addEventListener("message", wakeListener);

  const result = {
    enabled: false,
    reason: undefined,
    subscription: null,
    disable: null,
  };

  // 2-3. Permission + subscribe. Headless/embedded browsers without a push
  //     service reject here; that leaves the SW + wake listener live
  //     (reason recorded) rather than failing the whole enablement.
  if (typeof Notification === "undefined") {
    result.reason = "Notification API unavailable";
  } else {
    const permission = await Notification.requestPermission();
    if (permission !== "granted") {
      result.reason = "notification permission " + permission;
    } else {
      try {
        const manager = registration.pushManager;
        result.subscription =
          (await manager.getSubscription()) ||
          (await manager.subscribe({
            userVisibleOnly: true,
            applicationServerKey: urlB64ToUint8Array(opts.vapidPublicKey),
          }));
      } catch (e) {
        result.reason = "subscribe failed: " + ((e && e.message) || String(e));
      }
    }
  }

  // 4. REST registration (only when we hold a subscription).
  if (result.subscription) {
    await registerSubscription(result.subscription, cfg);
    result.enabled = true;
  }

  // Sign-out hook: unsubscribe + deregister + drop the wake listener.
  result.disable = async () => {
    navigator.serviceWorker.removeEventListener("message", wakeListener);
    const sub = result.subscription || (await registration.pushManager.getSubscription());
    if (sub) {
      try {
        await deregisterSubscription(sub, cfg);
      } finally {
        await sub.unsubscribe();
      }
    }
    result.enabled = false;
    result.subscription = null;
  };

  return result;
}
