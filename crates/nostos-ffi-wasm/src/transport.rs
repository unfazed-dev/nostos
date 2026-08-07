//! The WASM WebSocket transport — pure frame-pump + thin `web_sys` glue.
//!
//! Two layers, split by testability (see the crate docs for the rationale):
//!
//! - **Pure, host-unit-tested**: [`WireFrame`] + [`decode_frames`] (the mirror
//!   of `nostos_infra::wire::decode_frames`), [`decode_hex`],
//!   [`build_subscribe_frame`], [`build_ack_frame`], the frame-pump
//!   [`on_message`] → [`PumpResult`], and the [`checkpoint_key`] /
//!   [`parse_checkpoint`] helpers. These are the real coverage — every wire
//!   shape and apply/ack/checkpoint transition runs in `make ci`.
//! - **Browser glue, NOT host-tested**: [`read_checkpoint_ls`] /
//!   [`write_checkpoint_ls`] (localStorage), [`yield_to_event_loop`], and the
//!   [`connect`](crate::NostosSocket::connect) async fn + [`SocketInner`]
//!   (the `web_sys::WebSocket` plumbing). A browser can't be spawned in CI
//!   without a flaky headless harness; the glue is plumbing over the tested
//!   pump. Covered by the E3 demo page manual check
//!   (ponytail: WS glue untested in CI).
//!
//! ## Why mirror, not import
//!
//! `nostos-infra` owns the canonical wire codec, but it pulls in tokio, axum,
//! and tokio-postgres — none WASM-clean. The decode surface here is the tiny
//! twin: just enough to read an inbound event frame and its batched-array form.
//! The outbound `subscribe`/`ack` shapes are built with serde so they match
//! `ClientMessage`'s `#[serde(tag="type", rename_all="lowercase")]` tag exactly
//! (a hand-rolled JSON string would drift silently and close the server socket).

use nostos_core::{Frame as CoreFrame, Operation, Outbox};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{closure::Closure, prelude::*};
use wasm_bindgen_futures::JsFuture;

use crate::NostosEngine;

// -----------------------------------------------------------------------------
// Wire decode (mirror of nostos_infra::wire) — pure, host-tested.
// -----------------------------------------------------------------------------

/// One inbound event frame on the wire (server → client), the WASM twin of
/// `nostos_infra::wire::WireFrame`. `payload` is the hex-encoded opaque tuple
/// image (`None` for deletes); we hex-decode it once, at the boundary
/// ([`decode_hex`]), before handing a [`nostos_core::Frame`] to the engine.
///
/// `op` reuses [`nostos_domain::Operation`], which serializes to
/// `insert`/`update`/`delete` via its own `#[serde(rename_all="lowercase")]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireFrame {
    pub lsn: u64,
    pub op: Operation,
    pub table: String,
    pub pk: String,
    /// Hex-encoded payload; absent/null for deletes. Defaults to `None`.
    #[serde(default)]
    pub payload: Option<String>,
    /// Optional transaction id; absent for standalone frames. Defaults to `None`.
    #[serde(default)]
    pub txn_id: Option<u64>,
}

/// Decode a WebSocket message into zero or more frames (C3 batched-writes).
///
/// Accepts BOTH wire forms so the server can batch without a version bump:
/// - **single object** `{...}` → one frame, and
/// - **JSON array** `[{...},{...}]` → N frames.
///
/// Returns an empty `Vec` on a malformed message (the pump ignores it,
/// matching `decode_client_message`'s "drop malformed" behavior). O(1)
/// dispatch: peeks the first significant byte (`[` → array, `{` → object)
/// rather than parsing twice. This is the verbatim mirror of
/// `nostos_infra::wire::decode_frames`.
#[must_use]
pub fn decode_frames(data: &[u8]) -> Vec<WireFrame> {
    let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
    match first {
        Some(b'[') => serde_json::from_slice::<Vec<WireFrame>>(data).unwrap_or_default(),
        Some(b'{') => match serde_json::from_slice::<WireFrame>(data) {
            Ok(f) => vec![f],
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Decode a hex string to bytes. The wire payload is hex-encoded (see
/// `nostos_infra::wire::encode_event`); we decode once at the client boundary so
/// downstream everything is raw `Vec<u8>`. Returns `None` on odd-length or
/// non-hex input. Mirror of `nostos_client::client::decode_hex`.
#[must_use]
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// -----------------------------------------------------------------------------
// Wire encode (subscribe / ack) — pure, host-tested.
// -----------------------------------------------------------------------------

/// The serde shape of the outbound `subscribe` frame. Built directly with the
/// right field set so the JSON matches `ClientMessage::Subscribe` byte-for-byte:
/// `{"type":"subscribe","table":..,"filters":[],"resume_lsn":..,"where_sql":..}`.
/// `filters` is always `[]` (the browser path uses `where_sql`, not column
/// filters). `resume_lsn` / `where_sql` are omitted when `None`, mirroring the
/// `#[serde(skip_serializing_if = "Option::is_none")]` on `ClientMessage`.
#[derive(Serialize)]
struct SubscribeFrame<'a> {
    #[serde(rename = "type")]
    typ: &'a str,
    table: &'a str,
    filters: &'a [FilterClause],
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_lsn: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    where_sql: Option<&'a str>,
}

/// Marker for the `filters` array. The browser transport sends `[]` (it uses
/// `where_sql` exclusively), but the field MUST be present on the wire or
/// `decode_client_message` rejects the frame.
#[derive(Serialize)]
struct FilterClause;

/// Build the outbound `subscribe` frame as a JSON string, exactly the shape the
/// server's `decode_client_message` accepts:
///
/// ```json
/// {"type":"subscribe","table":"<table>","filters":[],"resume_lsn":<u64>?,"where_sql":"<pred>"?}
/// ```
///
/// `resume_lsn` is included only when `Some`; `where_sql` only when `Some`
/// (and non-empty). `filters` is always `[]` (the safe-SQL `where_sql` is the
/// browser's filter mechanism — see ADR-0012). Mirrors
/// `nostos_client::SyncClient`'s subscribe construction, byte-for-byte.
#[must_use]
pub fn build_subscribe_frame(
    table: &str,
    where_sql: Option<&str>,
    resume_lsn: Option<u64>,
) -> String {
    let frame = SubscribeFrame {
        typ: "subscribe",
        table,
        filters: &[],
        resume_lsn,
        where_sql: where_sql.filter(|s| !s.is_empty()),
    };
    // Safe: the fields are all JSON-serializable primitives.
    serde_json::to_string(&frame).expect("subscribe frame must serialize")
}

/// The serde shape of the outbound `ack` frame:
/// `{"type":"ack","lsn":<u64>}`.
#[derive(Serialize)]
struct AckFrame {
    #[serde(rename = "type")]
    typ: &'static str,
    lsn: u64,
}

/// Build the outbound `ack` frame as a JSON string: `{"type":"ack","lsn":<u64>}`.
/// Sent after each applied batch to drive the ack-driven slot advance (ADR-0009).
#[must_use]
pub fn build_ack_frame(lsn: u64) -> String {
    let frame = AckFrame { typ: "ack", lsn };
    serde_json::to_string(&frame).expect("ack frame must serialize")
}

// -----------------------------------------------------------------------------
// Wire encode (write) — pure, host-tested.
// -----------------------------------------------------------------------------

/// The serde shape of the outbound `write` frame, matching the
/// `ClientMessage::Write` variant in `nostos_infra::wire` byte-for-byte:
/// `{"type":"write","table":..,"op":..,"pk":..,"payload":..?,"client_write_id":..}`.
/// `payload` is omitted when `None` (deletes); otherwise it MUST be a JSON
/// object — the server's `ClientMessage::Write` rejects arrays / scalars /
/// null as `InvalidPayload` before any SQL is built. We parse the caller's
/// JSON string into a `serde_json::Value` here so a non-object surfaces as a
/// local error rather than a closed socket.
#[derive(Serialize)]
struct WriteFrame<'a> {
    #[serde(rename = "type")]
    typ: &'static str,
    table: &'a str,
    op: &'a str,
    pk: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
    client_write_id: &'a str,
}

/// Build the outbound `write` frame as a JSON string, exactly the shape the
/// server's `decode_client_message` accepts:
///
/// ```json
/// {"type":"write","table":"<t>","op":"upsert","pk":"<id>","payload":{...},"client_write_id":"<id>"}
/// ```
///
/// `payload_json` is the COLUMN→value tuple image (a JSON object) for upsert /
/// patch; `None` (or an empty string) for delete. Non-object payloads (arrays,
/// scalars, null) and malformed JSON return `Err` so the caller can surface it
/// rather than ship a frame the server will reject.
///
/// # Errors
/// - `Err(JsValue)` if `payload_json` is `Some` but not valid JSON
/// - `Err(JsValue)` if the parsed payload is not a JSON object (the server's
///   `ClientMessage::Write` rejects non-objects as `InvalidPayload`)
pub fn build_write_frame(
    table: &str,
    op: &str,
    pk: &str,
    payload_json: Option<&str>,
    client_write_id: &str,
) -> Result<String, JsValue> {
    let payload = match payload_json.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            let v: serde_json::Value = serde_json::from_str(s).map_err(|e| {
                JsValue::from_str(&format!("nostos write: invalid payload JSON: {e}"))
            })?;
            if !v.is_object() {
                return Err(JsValue::from_str(&format!(
                    "nostos write: payload must be a JSON object (got {})", // Cheap summary: array / scalar / null — no PII.
                    match v {
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Null => "null",
                        _ => "scalar",
                    }
                )));
            }
            Some(v)
        }
    };
    let frame = WriteFrame {
        typ: "write",
        table,
        op,
        pk,
        payload,
        client_write_id,
    };
    serde_json::to_string(&frame)
        .map_err(|e| JsValue::from_str(&format!("nostos write: frame serialize: {e}")))
}

// -----------------------------------------------------------------------------
// The pure frame-pump — host-tested.
// -----------------------------------------------------------------------------

/// The result of pumping one inbound message through the apply engine. Returned
/// by [`on_message`] so the WS glue knows whether + what to ACK + persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PumpResult {
    /// Frames decoded + fed this message (including buffered-but-not-flushed).
    pub applied: usize,
    /// The LSN to ACK to the server, iff this message triggered a commit. The
    /// glue sends `ack` with this LSN and persists it to `localStorage`. `None`
    /// when every decoded frame was buffered pending a future boundary.
    pub ack: Option<u64>,
}

/// Pure frame-pump: decode bytes → feed frames → return what the WS layer should
/// ACK. Mirrors the receive loop in `nostos_client::SyncClient::run_once`:
/// decode (array OR single object) → for each frame, hex-decode the payload →
/// feed the engine → if the feed triggered a commit, capture its checkpoint as
/// the ack LSN.
///
/// A single message may yield multiple commits (e.g. a batched array that
/// straddles a transaction boundary); the LAST commit's checkpoint wins as the
/// ack (it's the highest, since the engine's high-water is monotonic). If no
/// frame committed, `ack` is `None` and the glue neither acks nor persists.
///
/// # Errors
/// Surface storage errors verbatim as `Err(JsValue)` so the glue can close the
/// socket. The in-memory backend never errors, but the contract is preserved
/// for the future OPFS backend (E2).
pub fn on_message(engine: &mut NostosEngine, bytes: &[u8]) -> Result<PumpResult, JsValue> {
    let frames = decode_frames(bytes);
    let count = frames.len();

    let mut result = PumpResult::default();
    for frame in frames {
        // Hex-decode the payload once, at the boundary (the wire carries hex).
        let payload = frame.payload.as_deref().and_then(decode_hex);

        let core_frame = CoreFrame {
            lsn: frame.lsn,
            op: frame.op,
            table: frame.table,
            pk: frame.pk,
            payload,
            txn_id: frame.txn_id,
        };

        // Feed the engine; capture the checkpoint if this frame committed.
        if let Some(outcome) = engine.feed_frame(core_frame)? {
            // Outcome.checkpoint() is f64 at the JS boundary; narrow to u64
            // (real LSNs stay well under 2^53 — see the crate-level cast allows).
            result.ack = Some(outcome.checkpoint() as u64);
        }
    }

    result.applied = count;
    Ok(result)
}

/// The new checkpoint to persist after a pump, or `None` if the message didn't
/// commit. Same as `result.ack` — kept as a named accessor so the glue's intent
/// ("what do I write to localStorage?") is self-documenting.
#[must_use]
pub fn checkpoint_from(result: PumpResult) -> Option<u64> {
    result.ack
}

/// Reactive emit trigger (ADR-0024): a WS message should push a fresh snapshot
/// iff its pump OR its trailing idle-flush committed at least one frame. This is
/// the WASM reactive primitive's pure decision core — the analog of node's
/// `SnapshotEmitter` / kotlin's `SnapshotSink` "fire on every change tick", but
/// in the browser the change tick IS the `on_message` pump (single-threaded,
/// cooperative event loop). nostos-ffi-wasm binds `nostos-core` — NOT
/// `nostos-client` — so there is no tokio `broadcast` channel to drain
/// (`SyncClient::subscribe_changes` is unavailable here; ADR-0017). The
/// `on_message` Closure calls this to decide whether to invoke the registered
/// snapshot callback, so the push can be proven in pure-Rust host tests without
/// a JS runtime (a `Closure` cannot be built/invoked without one — same
/// testability split that motivated node's `SnapshotEmitter` seam). Host-tested.
#[must_use]
pub fn pump_committed(pump: &PumpResult, flush_committed: bool) -> bool {
    pump.ack.is_some() || flush_committed
}

// -----------------------------------------------------------------------------
// Checkpoint key + (de)serialization — pure, host-tested.
// -----------------------------------------------------------------------------

/// The `localStorage` key under which the table's durable checkpoint lives.
/// Format: `cairn:checkpoint:<table>`. Survives reloads so a reconnect can
/// resume from `resume_lsn` (the rows themselves are in-memory until OPFS in
/// E2 — the ceiling is "reload replays from resume_lsn").
#[must_use]
pub fn checkpoint_key(table: &str) -> String {
    format!("cairn:checkpoint:{table}")
}

/// Parse a `localStorage` value into a checkpoint LSN. Accepts a bare decimal
/// integer (the form [`write_checkpoint_ls`] writes). Returns `None` on
/// missing/malformed, falling back to "resume from 0" on connect.
#[must_use]
pub fn parse_checkpoint(raw: Option<&str>) -> Option<u64> {
    raw?.trim().parse::<u64>().ok()
}

// -----------------------------------------------------------------------------
// localStorage helpers — web-sys glue, NOT host-tested (browser-only).
// -----------------------------------------------------------------------------

/// Read the persisted checkpoint for `table` from `localStorage`. Returns
/// `None` if the key is missing/malformed (the connect path then resumes from
/// the engine's current checkpoint, or 0). ponytail: WS glue untested in CI.
fn read_checkpoint_ls(table: &str) -> Option<u64> {
    let storage = window_local_storage()?;
    let key = checkpoint_key(table);
    let raw = storage.get_item(&key).ok().flatten();
    parse_checkpoint(raw.as_deref())
}

/// Persist `lsn` as the durable checkpoint for `table` in `localStorage`.
/// Idempotent: overwrites the prior value. ponytail: WS glue untested in CI.
fn write_checkpoint_ls(table: &str, lsn: u64) {
    if let Some(storage) = window_local_storage() {
        let _ = storage.set_item(&checkpoint_key(table), &lsn.to_string());
    }
}

/// Reach `window.localStorage`, or `None` if there's no window / storage
/// (e.g. in a Worker, or if the user disabled storage). The connect path
/// treats `None` as "no persisted checkpoint → resume from 0".
fn window_local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// Yield one turn to the browser event loop so an installed `onopen`/`onmessage`
/// callback can fire before the caller re-checks `ready_state`. ponytail: a
/// production connect awaits the `open` event via a oneshot channel rather than
/// polling `ready_state`; this resolve-on-next-tick is enough for the E3 demo
/// and for any single-tab caller. WS glue untested in CI.
fn resolved_promise() -> js_sys::Promise {
    // A resolved promise resolves on the next microtask — exactly one yield.
    js_sys::Promise::resolve(&JsValue::UNDEFINED)
}

// -----------------------------------------------------------------------------
// NostosSocket glue: connect + the per-message pump. web-sys, NOT host-tested.
// -----------------------------------------------------------------------------

/// The shared state behind a `NostosSocket`: the engine + the wire handle + the
/// table the pump acks/persists against. `Rc<RefCell<…>>` so each closure
/// installed on the `WebSocket` can borrow the engine without `&mut self`
/// threading through JS callbacks.
pub(crate) struct SocketInner {
    pub(crate) engine: Rc<RefCell<NostosEngine>>,
    pub(crate) ws: web_sys::WebSocket,
    pub(crate) table: String,
    /// Reactive push slot (ADR-0024): a no-arg `Closure` the `on_message` pump
    /// invokes on every commit ([`pump_committed`]). `None` until
    /// [`crate::NostosSocket::on_change`] registers one; cleared by
    /// [`crate::NostosSocket::off_change`] and dropped with the socket — no
    /// `.forget()`, so no leak (the one wasm-bindgen `Closure` pitfall). The
    /// callback is a change *tick*; the host re-reads `rows_for` inside it for
    /// the fresh full-table snapshot (idempotent — self-healing on lag, like the
    /// node/kotlin ports).
    pub(crate) on_change: OnChangeSlot,
}

/// The reactive push slot's type: an optional no-arg `Closure` behind shared
/// interior mutability. Factored out so the field + [`emit_change`] share one
/// spelling (clippy `type_complexity`), and so the "drop to detach, no
/// `.forget()`" ownership contract lives in one named place.
pub(crate) type OnChangeSlot = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// Drain the outbox over an OPEN socket: send each pending write frame and
/// `mark_done` on success. Called from the `onopen` handler so writes captured
/// while disconnected ship as soon as the connection (re)establishes — the
/// "flush loop drains pending writes when connected" half of the WS1 write
/// contract. A send failure (socket closed mid-drain) leaves the write pending
/// for the next open. ponytail: WS glue untested in CI; covered by the manual /
/// E3 path (the live path that exercises this needs a disconnect-mid-write
/// harness, out of scope for slice 2).
fn flush_pending(inner: &Rc<SocketInner>) {
    // Snapshot the pending list (owned Vec) so the RefCell borrow is released
    // before we re-borrow for mark_done inside the loop.
    let Ok(pending) = inner.engine.borrow_mut().storage_mut().pending() else {
        return;
    };
    for (id, write) in pending {
        if inner.ws.ready_state() != 1 {
            break; // closed mid-drain — leave the rest pending for next open
        }
        let sent = build_write_frame(
            &write.table,
            write.op.as_wire_str(),
            &write.pk,
            write.payload_json.as_deref(),
            &id.to_string(),
        )
        .is_ok_and(|f| inner.ws.send_with_str(&f).is_ok());
        if sent {
            let _ = inner.engine.borrow_mut().storage_mut().mark_done(id);
        }
    }
}

/// Invoke the registered reactive tick callback (if any) — the Rust→JS push.
/// Fire-and-forget: a JS error (the side tearing down) is swallowed;
/// `off_change` / socket `Drop` is the only true end. ponytail: WS glue is
/// untested in CI (`JsValue` is browser-only); the commit trigger
/// ([`pump_committed`]) + snapshot shape (`NostosEngine::rows_for`) are the
/// host-tested pure cores this pushes from.
pub(crate) fn emit_change(slot: &OnChangeSlot) {
    if let Some(cb) = slot.borrow().as_ref() {
        // `Closure: AsRef<JsValue>` → `unchecked_ref::<Function>` → `call0`.
        let _ = cb
            .as_ref()
            .unchecked_ref::<js_sys::Function>()
            .call0(&JsValue::UNDEFINED);
    }
}

/// Connect to `url`, await the browser's `open`, then resolve. Called by the
/// `#[wasm_bindgen] async fn` `NostosSocket::connect`. ponytail: WS glue
/// untested in CI; covered by the E3 demo page manual check.
///
/// The connect flow mirrors `nostos_client::SyncClient::run_once`'s handshake:
/// build the `?token=` URL → open the socket → set `BinaryType::Arraybuffer` →
/// install the four handlers → send `subscribe` on open → return the socket.
///
/// # Errors
/// Rejects (returns `Err`) if the browser refuses the WebSocket URL or the
/// socket closes before reaching OPEN.
pub(crate) async fn connect(
    url: String,
    token: Option<String>,
    table: String,
    where_sql: Option<String>,
    db_handle: Option<js_sys::Object>,
) -> Result<crate::NostosSocket, JsValue> {
    // Build the connect URL with `?token=` (same convention as the native
    // SyncClient — browsers can't set headers on a WS handshake).
    let connect_url = match &token {
        Some(t) if !t.is_empty() => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}token={t}")
        }
        _ => url,
    };

    let ws = web_sys::WebSocket::new(&connect_url)?;
    // Receive events as ArrayBuffer so the pump gets &[u8] directly (the server
    // sends binary frames; text would force a UTF-8 round-trip).
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    // Resume from the persisted checkpoint. For the durable (SqliteWasm) path,
    // the engine's storage has the checkpoint from SQLite (survives reload —
    // ADR-0033). For the in-memory path, read `localStorage` (today's behavior,
    // the degrade fallback). `db_handle.is_some()` = durable mode.
    let where_sql = where_sql.filter(|s| !s.is_empty());

    let mut engine = match &db_handle {
        Some(db) => NostosEngine::with_durable(db.clone()),
        None => NostosEngine::new(),
    };
    engine.set_where_sql(where_sql.clone());

    // For durable mode, the checkpoint comes from SQLite (the engine already
    // loaded it at construction). For in-memory, fall back to localStorage.
    let resume_lsn = if engine.storage().is_durable() {
        let cp = engine.checkpoint();
        if cp > 0.0 {
            Some(cp as u64)
        } else {
            None
        }
    } else {
        read_checkpoint_ls(&table)
    };
    let inner = Rc::new(SocketInner {
        engine: Rc::new(RefCell::new(engine)),
        ws: ws.clone(),
        table: table.clone(),
        on_change: Rc::new(RefCell::new(None)),
    });

    // --- onopen: send the subscribe frame (the server won't stream until it
    //     decodes a valid subscribe). ---
    let inner_open = Rc::clone(&inner);
    let table_open = table.clone();
    let resume_open = resume_lsn;
    let where_open = where_sql;
    let on_open = Closure::new(move |_evt: JsValue| {
        let frame = build_subscribe_frame(&table_open, where_open.as_deref(), resume_open);
        // Sending can fail only if the socket closed between open + send; ignore
        // — onclose will run.
        let _ = inner_open.ws.send_with_str(&frame);
        // Flush loop (WS1): drain writes enqueued while the socket was closed.
        // They were apply_local'd on enqueue (instant local row); this only
        // ships them. mark_done on send success. ponytail: PendingWrite carries
        // no client_write_id field, so the wire id is synthesized from the
        // outbox id here — the caller's id is preserved only on the live send
        // path (NostosSocket::write).
        flush_pending(&inner_open);
    });

    // --- onmessage: the pure frame-pump → idle-flush → persist checkpoint → ack. ---
    let inner_msg = Rc::clone(&inner);
    let on_message = Closure::new(move |evt: web_sys::MessageEvent| {
        let bytes = message_bytes(&evt);
        let mut engine = inner_msg.engine.borrow_mut();
        // A backend error is non-recoverable on the in-memory backend (it never
        // errors); on a future OPFS backend, close + reconnect. 1011 = "internal
        // error". ponytail: no retry/backoff — the E3 demo reloads the page; a
        // production client adds reconnect logic.
        let Ok(pump) = on_message(&mut engine, &bytes) else {
            let _ = inner_msg.ws.close_with_code(1011);
            return;
        };
        // Idle-flush: the engine buffers standalone frames (no `txn_id`) until
        // either a transaction boundary, the 256-frame soft cap, or an explicit
        // flush — the WS transport sees one Nostos event per WS message (or a
        // complete array), so commit any pending at message end. Mirrors what
        // the native `SyncClient::run_once` does via `flush_quiesce` on an idle
        // stream, but per-message here because the browser event loop delivers
        // each WS message as a discrete, atomic unit. `flush()` is a no-op
        // (returns `None`) when nothing is pending, so unconditional is safe.
        let mut ack_lsn = pump.ack;
        let mut flush_committed = false;
        if let Ok(Some(outcome)) = engine.flush() {
            // The pump may have already committed mid-message (transaction
            // boundary); the flush is a no-op in that case. Either way, the
            // last checkpoint wins (the engine's high-water is monotonic).
            ack_lsn = Some(outcome.checkpoint() as u64);
            flush_committed = true;
        }
        // Release the engine borrow BEFORE persist/ack/emit: none of them need
        // it, and releasing here lets the reactive callback re-enter the socket
        // (e.g. rowsFor) without panicking on a double `borrow_mut`.
        drop(engine);
        if let Some(lsn) = ack_lsn {
            // Persist FIRST (so a crash between ack + persist doesn't lose the
            // checkpoint and force a full replay), then tell the server.
            write_checkpoint_ls(&inner_msg.table, lsn);
            let _ = inner_msg.ws.send_with_str(&build_ack_frame(lsn));
        }
        // Reactive push (ADR-0024): on every change tick — a commit detected by
        // the pump OR the trailing flush — fire the registered callback. This is
        // the TRUE Rust→JS push (synchronous from the WS frame pump), NOT a
        // `setInterval` poll. There is no tokio broadcast to drain here (see
        // `pump_committed`); the `on_message` pump IS the change tick. The
        // callback is a no-arg tick; the host re-reads `rows_for` for the fresh
        // full-table snapshot.
        if pump_committed(&pump, flush_committed) {
            emit_change(&inner_msg.on_change);
        }
    });

    // --- onerror: web-sys gives no useful detail in ErrorEvent; onclose will
    //     run next and flush + persist the in-flight batch. ---
    let inner_err = Rc::clone(&inner);
    let on_error = Closure::new(move |_evt: web_sys::ErrorEvent| {
        // Closing here guarantees onclose fires even if the server didn't send
        // a Close frame (a hard transport error). ponytail: log in production.
        let _ = inner_err.ws.close();
    });

    // --- onclose: final flush of any buffered batch, then persist + ack. ---
    let inner_close = Rc::clone(&inner);
    let on_close = Closure::new(move |_evt: web_sys::CloseEvent| {
        let mut engine = inner_close.engine.borrow_mut();
        if let Ok(Some(outcome)) = engine.flush() {
            let lsn = outcome.checkpoint() as u64;
            write_checkpoint_ls(&inner_close.table, lsn);
            // The socket is closing; the ack may not land, but the persisted
            // checkpoint drives the next connect's resume_lsn regardless.
            let _ = inner_close.ws.send_with_str(&build_ack_frame(lsn));
        }
    });

    // Install the handlers. `as_ref().unchecked_ref()` turns the typed Closure
    // into the JS Function the setter wants; the Closure itself is owned by the
    // socket (kept alive for the WS's lifetime).
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));

    // Wait for the socket to OPEN before resolving (so the caller's `await`
    // returns a ready socket). Poll ready_state, yielding to the event loop
    // between checks so the browser can fire `open`. ponytail: polling
    // ready_state is crude; a production build awaits the open event through a
    // oneshot channel wired into the onopen closure. Sufficient for E3.
    //
    // ready_state: 0=CONNECTING, 1=OPEN, 2=CLOSING, 3=CLOSED. The 1000-iter
    // bound is ~17s at one yield-per-tick — ample for a localhost handshake.
    for _ in 0..1000 {
        match ws.ready_state() {
            1 => break,                           // OPEN
            3 => return Err(close_before_open()), // CLOSED — handshake failed
            _ => {}                               // CONNECTING / CLOSING — keep waiting
        }
        let _ = JsFuture::from(resolved_promise()).await;
    }

    Ok(crate::NostosSocket::from_inner(
        inner, on_open, on_message, on_error, on_close,
    ))
}

/// A JsValue error for "the socket closed before reaching OPEN".
fn close_before_open() -> JsValue {
    JsValue::from_str("nostos: WebSocket closed before OPEN (handshake failed)")
}

/// Coerce a `MessageEvent`'s `data` to owned bytes. Binary frames (ArrayBuffer)
/// slice directly; text frames UTF-8-encode (the server sends binary, but this
/// keeps a text fallback from panicking). ponytail: WS glue untested in CI.
fn message_bytes(evt: &web_sys::MessageEvent) -> Vec<u8> {
    let data = evt.data();
    if let Some(ab) = data.dyn_ref::<js_sys::ArrayBuffer>() {
        return js_sys::Uint8Array::new(ab).to_vec();
    }
    if let Some(s) = data.as_string() {
        return s.into_bytes();
    }
    Vec::new()
}

// Extension so connect() can build a NostosSocket from its private pieces.
impl crate::NostosSocket {
    pub(crate) fn from_inner(
        inner: Rc<SocketInner>,
        on_open: Closure<dyn FnMut(JsValue)>,
        on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
        on_error: Closure<dyn FnMut(web_sys::ErrorEvent)>,
        on_close: Closure<dyn FnMut(web_sys::CloseEvent)>,
    ) -> Self {
        Self {
            inner,
            on_open: Some(on_open),
            on_message: Some(on_message),
            on_error: Some(on_error),
            on_close: Some(on_close),
        }
    }
}
