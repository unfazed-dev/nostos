//! # nostos-ffi-wasm — the WebAssembly bridge over `nostos-core`.
//!
//! A thin `#[wasm_bindgen]` projection of the apply engine for JavaScript.
//! The engine, atomic batching, and idempotency all live in `nostos-core` (pure
//! Rust, tested by 18 unit tests); this crate only adapts the public surface to
//! JS-friendly types. No new logic.
//!
//! ## Scope (ADR-0015)
//!
//! This slice exposes the **in-memory apply path** AND the **browser WebSocket
//! transport** (E1): build an engine, connect a `NostosSocket`, and frames flow
//! in → applied → acked → checkpoint persisted to `localStorage`. It proves the
//! WASM bundle stays under budget (ADR-0015's kill criterion) and that the
//! JS↔Rust boundary works end-to-end.
//!
//! What's NOT here (ponytail — deferred):
//! - **OPFS persistence** — the browser-durable backend needs a Web Worker +
//!   sync-OPFS plumbing (Worker-only by spec); deferred past v0.1 per ADR-0017
//!   (decision: ship localStorage checkpoint + replay-from-resume_lsn now;
//!   adopt SQLite-WASM `opfs-sahpool` post-launch — no COOP/COEP tax).
//!   The ceiling today is "reload replays from `resume_lsn`" — the
//!   `localStorage` checkpoint survives, the in-memory rows don't.
//! - **The browser WS glue's automated test** — `web_sys::WebSocket` can't run
//!   headless in CI without a flaky browser harness; the pure frame-pump is
//!   host-tested, the glue is covered by the E3 demo page manual check.
//! - **Flutter / RN / Node-native bridges** — the other FFI targets.
//!
//! ## JS type ergonomics
//!
//! `u64` LSNs are exposed as `f64` at the JS boundary (no BigInt gymnastics):
//! real WAL positions stay well under 2^53 bits of precision, and `Number` is
//! what every JS caller has in hand. `Vec<u8>` payloads map to `Uint8Array`.
//!
//! ## JS usage
//!
//! ```js
//! import init, { NostosEngine, Frame } from "nostos-ffi-wasm";
//! await init();
//! const eng = new NostosEngine();
//! eng.feed(new Frame(10, "insert", "tasks", "1", new Uint8Array([1,2,3])));
//! eng.flush();
//! console.log(eng.checkpoint, eng.rowCount);  // 10, 1
//! ```

#![forbid(unsafe_code)]
// FFI boundary: LSNs cross to JavaScript as `f64` (no BigInt gymnastics). The
// cast lints fire on the intentional `f64 as u64` / `u64 as f64` round-trip;
// real WAL positions stay well under 2^53 bits, so the precision-loss/truncation
// lints don't apply. Mirrors the cast allows in nostos-bench's reporting code.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use nostos_core::{
    ApplyEngine, ApplyOutcome, Frame as CoreFrame, InMemoryStorage, Lsn, Operation, Outbox,
    PendingWrite, WriteOp,
};
use wasm_bindgen::prelude::*;

/// The operation kind, as a JS-friendly string. Matches `nostos_domain::Operation`.
///
/// JS passes one of `"insert" | "update" | "delete"`. Any other value defaults
/// to `insert` (the common case) rather than throwing — a malformed op string
/// produces a no-op-equivalent row, not a crashed WASM instance.
fn parse_op(s: &str) -> Operation {
    match s.to_ascii_lowercase().as_str() {
        "update" => Operation::Update,
        "delete" => Operation::Delete,
        _ => Operation::Insert,
    }
}

/// A replication frame, mirrored from `nostos_core::Frame` into JS-friendly types.
///
/// `payload` is an optional `Uint8Array`-backed `Vec<u8>` (the opaque tuple
/// image); `None`/null/undefined for deletes. `lsn` is `f64` (see module docs).
#[wasm_bindgen]
pub struct Frame {
    lsn: u64,
    op: Operation,
    table: String,
    pk: String,
    payload: Option<Vec<u8>>,
    txn_id: Option<u64>,
}

#[wasm_bindgen]
impl Frame {
    /// Build a frame from JS. `op` is `"insert" | "update" | "delete"`.
    /// `payload` may be null/undefined (deletes); `txn_id` may be null/undefined.
    ///
    /// `lsn` and `txn_id` are `f64` to avoid BigInt at the JS boundary; they're
    /// narrowed to `u64` internally (real LSNs never approach 2^53).
    #[wasm_bindgen(constructor)]
    pub fn new(
        lsn: f64,
        op: &str,
        table: &str,
        pk: &str,
        payload: Option<Vec<u8>>,
        txn_id: Option<f64>,
    ) -> Self {
        Self {
            lsn: lsn as u64,
            op: parse_op(op),
            table: table.to_owned(),
            pk: pk.to_owned(),
            payload,
            txn_id: txn_id.map(|t| t as u64),
        }
    }
}

/// Convert the JS `Frame` into the pure-Rust `nostos_core::Frame` the engine consumes.
impl From<Frame> for CoreFrame {
    fn from(f: Frame) -> Self {
        CoreFrame {
            lsn: f.lsn,
            op: f.op,
            table: f.table,
            pk: f.pk,
            payload: f.payload,
            txn_id: f.txn_id,
        }
    }
}

/// The result of an atomic commit, mirrored to JS.
#[wasm_bindgen]
pub struct Outcome {
    checkpoint: u64,
    rows_applied: usize,
}

#[wasm_bindgen]
impl Outcome {
    /// The new durable checkpoint — the value to `Ack` to the server.
    #[wasm_bindgen(getter)]
    pub fn checkpoint(&self) -> f64 {
        self.checkpoint as f64
    }

    /// Rows applied in this commit.
    #[wasm_bindgen(getter, js_name = rowsApplied)]
    pub fn rows_applied(&self) -> usize {
        self.rows_applied
    }
}

impl From<ApplyOutcome> for Outcome {
    fn from(o: ApplyOutcome) -> Self {
        Self {
            checkpoint: o.checkpoint.raw(),
            rows_applied: o.rows_applied,
        }
    }
}

/// One `(pk, payload)` pair returned by [`NostosEngine::rows_for`]. The JS-facing
/// projection of `InMemoryStorage`'s readback — `pk` is the row's primary key,
/// `payload` is the opaque tuple image (the bytes the engine applied), exposed
/// as a `Uint8Array` (matches the `Frame` payload convention).
///
/// Not constructable from JS: instances only flow OUT of the engine (the engine
/// is the source of truth for row state). JS reads `entry.pk` / `entry.payload`.
#[wasm_bindgen]
pub struct RowEntry {
    pk: String,
    payload: Vec<u8>,
}

#[wasm_bindgen]
impl RowEntry {
    /// The row's primary key.
    #[wasm_bindgen(getter)]
    pub fn pk(&self) -> String {
        self.pk.clone()
    }

    /// The opaque tuple image bytes (decode/interpret on the JS side).
    #[wasm_bindgen(getter)]
    pub fn payload(&self) -> Vec<u8> {
        self.payload.clone()
    }
}

/// The Nostos apply engine, running in-memory in the browser.
///
/// Construct with `new NostosEngine()`. Feed frames; flush to commit a pending
/// batch; read `checkpoint` to drive `resume_lsn` on reconnect.
///
/// ## `where_sql` (ADR-0012)
///
/// The engine carries an optional `where_sql` predicate string
/// ([`NostosEngine::set_where_sql`]) that the WASM transport (E1) will attach to
/// the subscribe frame when it connects. The apply engine itself does NOT
/// evaluate it — the server compiles + ANDs it into the session predicate, so
/// only matching rows are ever sent. Storing it on the engine lets E1 read it
/// at connect time without a separate config object crossing the JS boundary.
#[wasm_bindgen]
pub struct NostosEngine {
    inner: ApplyEngine<InMemoryStorage>,
    /// The optional safe-SQL predicate for the next subscribe. Held here so the
    /// future WASM transport (E1) can read it when sending the subscribe frame;
    /// the in-memory apply path ignores it (the server filters upstream).
    where_sql: Option<String>,
}

#[wasm_bindgen]
impl NostosEngine {
    /// Create an in-memory engine. Data survives the apply loop but NOT a page
    /// reload — real browser persistence (OPFS) is deferred past v0.1 (ADR-0017).
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: ApplyEngine::new(InMemoryStorage::new()),
            where_sql: None,
        }
    }

    /// Set the `where_sql` predicate the transport (E1) will attach to the next
    /// subscribe frame — e.g. `"priority > 5"`. Pass `null`/`undefined` to clear
    /// it. The grammar is the safe-SQL subset (six comparison operators +
    /// `AND`/`OR`/`NOT` + parens); a parse failure closes the server socket with
    /// an `invalid where_sql:` reason before any event flows. The apply engine
    /// stores this for E1; it does not evaluate it locally (the server filters).
    ///
    /// JS:
    /// ```js
    /// const eng = new NostosEngine();
    /// eng.setWhereSql("status = open AND priority >= 3");
    /// ```
    #[wasm_bindgen(js_name = setWhereSql)]
    pub fn set_where_sql(&mut self, sql: Option<String>) {
        self.where_sql = sql.filter(|s| !s.is_empty());
    }

    /// The configured `where_sql`, or `null` if none. E1's transport reads this
    /// when building the subscribe frame.
    #[wasm_bindgen(getter, js_name = whereSql)]
    pub fn where_sql(&self) -> Option<String> {
        self.where_sql.clone()
    }

    /// Feed a frame. Returns an `Outcome` if the frame triggered a commit (a
    /// transaction boundary or the soft cap), or `undefined` if the frame was
    /// buffered pending a future boundary. Throws on a backend error (the
    /// in-memory backend never errors, but the contract is preserved).
    pub fn feed(&mut self, frame: Frame) -> Result<Option<Outcome>, JsValue> {
        match self.inner.feed(frame.into()) {
            Ok(Some(outcome)) => Ok(Some(outcome.into())),
            Ok(None) => Ok(None),
            Err(e) => Err(JsValue::from_str(&e.to_string())),
        }
    }

    /// Feed a decoded [`nostos_core::Frame`] directly (no JS `Frame` wrapper).
    /// This is the seam the WASM transport's frame-pump (`transport::on_message`)
    /// uses: it hex-decodes the wire payload into bytes once, then feeds the
    /// pure frame. The public JS `feed` does the same work through the JS `Frame`
    /// boundary; this variant skips that boundary for the in-Rust pump.
    ///
    /// Not exported to JS (no `#[wasm_bindgen]`) — it takes a non-JS type.
    pub(crate) fn feed_frame(
        &mut self,
        frame: nostos_core::Frame,
    ) -> Result<Option<Outcome>, JsValue> {
        match self.inner.feed(frame) {
            Ok(Some(outcome)) => Ok(Some(outcome.into())),
            Ok(None) => Ok(None),
            Err(e) => Err(JsValue::from_str(&e.to_string())),
        }
    }

    /// Flush any buffered frames as one atomic commit. Returns `undefined` if
    /// nothing was pending. Call this when the stream goes idle or the
    /// connection closes to make the last partial batch durable.
    pub fn flush(&mut self) -> Result<Option<Outcome>, JsValue> {
        match self.inner.flush() {
            Ok(Some(outcome)) => Ok(Some(outcome.into())),
            Ok(None) => Ok(None),
            Err(e) => Err(JsValue::from_str(&e.to_string())),
        }
    }

    /// The current durable checkpoint (the LSN to send as `resume_lsn` on a
    /// reconnect). 0 until the first commit.
    #[wasm_bindgen(getter)]
    pub fn checkpoint(&self) -> f64 {
        // The in-memory backend never errors; on the (impossible) error path,
        // report 0 rather than panic at the JS boundary.
        self.inner.checkpoint().map_or(0, Lsn::raw) as f64
    }

    /// How many rows the in-memory store currently holds.
    #[wasm_bindgen(getter, js_name = rowCount)]
    pub fn row_count(&self) -> usize {
        // Reach the concrete InMemoryStorage through the engine's read-only
        // accessor (the Storage trait itself has no row_count — it's not part of
        // the core contract; this is a JS/diagnostics convenience).
        self.inner.storage().row_count()
    }

    /// Mutable access to the backing `InMemoryStorage` — the outbox flush path
    /// (`Outbox::enqueue` / `apply_local` / `pending` / `mark_done`) reaches the
    /// store through this. `pub(crate)` because the JS surface never mutates
    /// storage directly; only [`NostosSocket`]'s write/flush path (WS1) does.
    pub(crate) fn storage_mut(&mut self) -> &mut InMemoryStorage {
        self.inner.storage_mut()
    }

    /// Enumerate the `(pk, payload)` pairs the engine currently holds for
    /// `table`, sorted by pk. The readback the browser demo renders from: each
    /// entry's `payload` is a `Uint8Array` (the opaque tuple image the engine
    /// applied); decode/interpret on the JS side.
    ///
    /// This is a JS/diagnostics convenience — NOT part of the `Storage` trait
    /// (the trait stays minimal: `checkpoint` + `apply_batch`). It reaches the
    /// concrete `InMemoryStorage` through the engine's read-only accessor.
    /// Deletes are excluded (a delete removes the row from the store, so its pk
    /// is absent); the enumeration reflects the engine's *current* state, not
    /// its event history.
    ///
    /// JS:
    /// ```js
    /// for (const entry of eng.rowsFor("tasks")) {
    ///   console.log(entry.pk, entry.payload);  // string, Uint8Array
    /// }
    /// ```
    #[wasm_bindgen(js_name = rowsFor)]
    pub fn rows_for(&self, table: &str) -> Vec<RowEntry> {
        self.inner
            .storage()
            .rows_for(table)
            .into_iter()
            .map(|(pk, payload)| RowEntry { pk, payload })
            .collect()
    }

    /// ADR-0029 D1: wipe the in-memory rows AND outbox — the sign-out
    /// local-state wipe for the browser. The `NostosEngine` has no checkpoint
    /// file; this clears the live in-memory store so the next user (same
    /// Worker/page session) does not see the previous user's rows. Call before
    /// `NostosSocket::close` on sign-out.
    pub fn clear(&mut self) {
        let s = self.inner.storage_mut();
        // Both clears under one borrow — half a clear is a cross-user leak.
        // ponytail: `let _ =` — `InMemoryStorage::clear` is infallible in
        // practice (a `HashMap`/`Vec` clear); the `Result` is just the trait
        // shape, surfaced as infallible here.
        let _ = <InMemoryStorage as nostos_core::Storage>::clear(s);
        let _ = <InMemoryStorage as Outbox>::clear(s);
    }
}

impl Default for NostosEngine {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// E1: the WASM WebSocket transport.
// =============================================================================
//
// Two layers, deliberately split by testability:
//
// 1. **The pure frame-pump** (`transport` module below) — decode a WS message's
//    bytes → feed frames → flush → tell the caller what to ACK + persist. Host-
//    unit-tested in `#[cfg(test)]` (runs in `make ci`). This is the real
//    coverage: every wire shape, every apply/ack/checkpoint transition.
//
// 2. **The `web_sys::WebSocket` glue** (`NostosSocket`) — connect, wire the
//    pump into `onmessage`, send subscribe/ack frames, persist the checkpoint
//    to `localStorage`. Thin and NOT host-tested: a browser can't be spawned
//    in CI without a flaky headless harness, and the glue is just plumbing
//    over the tested pump. Covered by the E3 demo page manual check
//    (ponytail: WS glue untested in CI).
//
// The wire format is MIRRORED from `nostos-infra::wire`, not imported —
// `nostos-infra` is NOT WASM-clean (tokio, axum, tokio-postgres). The decode
// surface here is the tiny twin of `decode_frames` + the `WireFrame` struct;
// the outbound `subscribe`/`ack` shapes are built with serde to match
// `ClientMessage`'s `#[serde(tag="type", rename_all="lowercase")]` tag exactly.

/// The WASM WebSocket transport: pure frame-pump + thin `web_sys` glue.
pub mod transport;

/// A live WebSocket sync session in the browser.
///
/// Construct with [`NostosSocket::connect`], which returns a `Promise` that
/// resolves to the socket once the browser has opened it and the subscribe
/// frame is queued (sent on `open`). The server then streams events; each
/// inbound message is decoded by the pure frame-pump, applied to the socket's
/// engine, ACKed per committed batch, and the resulting checkpoint is
/// persisted to `localStorage` under the `cairn:checkpoint:<table>` key so a
/// reload can resume.
///
/// ## Resume
///
/// On `connect`, `resume_lsn` is read from `localStorage` (falling back to 0)
/// and attached to the subscribe frame. The server skips re-delivering anything
/// ≤ that LSN.
///
/// ## What's NOT durable (ponytail)
///
/// Only the checkpoint survives a reload — the applied rows live in the
/// engine's `InMemoryStorage` and are lost on reload, so a reconnect replays
/// from `resume_lsn`. Durable rows arrive with OPFS post-v0.1 (ADR-0017).
///
/// ## JS
///
/// ```js
/// const sock = await NostosSocket.connect(
///   "ws://localhost:8080/sync", "tok", "tasks", "priority > 5"
/// );
/// // rows flow in; checkpoint persists to localStorage["cairn:checkpoint:tasks"]
/// console.log(sock.checkpoint, sock.rowCount);
/// sock.close();
/// ```
#[wasm_bindgen]
pub struct NostosSocket {
    inner: Rc<transport::SocketInner>,
    // The closures are kept alive on the socket so they outlive `connect`'s
    // stack frame — without this ownership, wasm-bindgen drops each Closure
    // (and detaches its JS callback) the moment `connect` returns, so the
    // socket stops firing. They're never *read* after construction; their
    // mere presence on the struct is what keeps the WS callbacks live. Each
    // captures a clone of `inner`; the socket owns `inner` too, so dropping
    // the socket drops every clone → the `Rc` cycle-free.
    #[allow(dead_code)]
    on_open: Option<Closure<dyn FnMut(JsValue)>>,
    #[allow(dead_code)]
    on_message: Option<Closure<dyn FnMut(web_sys::MessageEvent)>>,
    #[allow(dead_code)]
    on_error: Option<Closure<dyn FnMut(web_sys::ErrorEvent)>>,
    #[allow(dead_code)]
    on_close: Option<Closure<dyn FnMut(web_sys::CloseEvent)>>,
}

#[wasm_bindgen]
impl NostosSocket {
    /// Connect to `url`, await the browser's `open`, then resolve. JS sees an
    /// `async` fn, so `await NostosSocket.connect(...)` returns the ready socket.
    /// The subscribe frame is sent in the `onopen` handler; inbound frames flow
    /// into the socket's engine, are acked per committed batch, and the
    /// checkpoint is persisted to `localStorage[cairn:checkpoint:<table>]`.
    ///
    /// `token` is appended as `?token=` on the URL (browsers can't set headers
    /// on a WS handshake — same convention as the native `SyncClient`).
    /// `table` is the table to subscribe; `where_sql` is the optional safe-SQL
    /// predicate (cleared if empty/`null`). `resume_lsn` is read from
    /// `localStorage[cairn:checkpoint:<table>]`, falling back to 0.
    ///
    /// # Errors
    /// The `Promise` rejects if the browser can't open the socket (e.g. mixed
    /// content) or the handshake fails before OPEN.
    #[wasm_bindgen]
    pub async fn connect(
        url: String,
        token: Option<String>,
        table: String,
        where_sql: Option<String>,
    ) -> Result<NostosSocket, JsValue> {
        transport::connect(url, token, table, where_sql).await
    }

    /// The current durable checkpoint (the LSN persisted to `localStorage`).
    /// Mirrors `NostosEngine::checkpoint`.
    #[wasm_bindgen(getter)]
    pub fn checkpoint(&self) -> f64 {
        self.inner.engine.borrow().checkpoint()
    }

    /// Rows the in-memory store currently holds. Mirrors `NostosEngine::row_count`.
    #[wasm_bindgen(getter, js_name = rowCount)]
    pub fn row_count(&self) -> usize {
        self.inner.engine.borrow().row_count()
    }

    /// Enumerate the `(pk, payload)` pairs the socket's engine holds for
    /// `table`. Mirrors `NostosEngine::rows_for` — the readback the demo renders
    /// from. Safe because WASM is single-threaded and the JS event loop is
    /// cooperative — `setInterval(snapshot, …)` and the WS `onmessage` pump
    /// never run concurrently, so the `borrow_mut()` in the pump
    /// (`transport.rs`) and this `borrow()` can't overlap (a `RefCell` panics
    /// on re-borrow mid-`borrow_mut`; it doesn't deadlock, but the
    /// cooperative-event-loop invariant is what keeps that from happening).
    #[wasm_bindgen(js_name = rowsFor)]
    pub fn rows_for(&self, table: &str) -> Vec<RowEntry> {
        self.inner.engine.borrow().rows_for(table)
    }

    /// Send a client write. WS1 contract: this NEVER throws because the socket
    /// is closed — a write while disconnected is captured into the `Outbox`
    /// (`enqueue`) and rendered locally right away (`apply_local`), so the row
    /// is visible INSTANTLY and the write ships on the next (re)connect via the
    /// `onopen` flush loop. The synchronous "socket not OPEN" throw is gone
    /// (ADR-0017 WS1; reviewer note #1).
    ///
    /// The call is still `Err` for a *caller bug* — a malformed / non-object
    /// `payload_json`, or an `op` outside `"upsert" | "delete" | "patch"`. Those
    /// return BEFORE anything is enqueued (an invalid write is not captured).
    ///
    /// When the socket IS open, the write ships immediately and is
    /// `mark_done`'d, so the connected path keeps the outbox drained. The
    /// caller learns the outcome asynchronously: the Worker host turns the
    /// `Ok(())` into a `writeResult{client_write_id, ok:true}` push (Rust can't
    /// `postMessage` to the main thread itself).
    ///
    /// `client_write_id` is the caller's correlation id, put on the wire when
    /// the write ships now. The offline flush loop synthesizes one from the
    /// outbox id (ponytail: `PendingWrite` — a `nostos-core` domain type —
    /// carries no `client_write_id` field, so the caller's id is lost across an
    /// offline gap; the live path preserves it).
    #[wasm_bindgen(js_name = write)]
    #[allow(clippy::needless_pass_by_value)] // wasm-bindgen JS boundary: owned Option<String>
    pub fn write(
        &self,
        table: &str,
        op: &str,
        pk: &str,
        payload_json: Option<String>,
        client_write_id: &str,
    ) -> Result<(), JsValue> {
        // Validate payload + build the wire frame FIRST (caller bug → Err, and
        // nothing is enqueued). `op` is validated just below.
        let frame =
            transport::build_write_frame(table, op, pk, payload_json.as_deref(), client_write_id)?;
        let op_enum = WriteOp::from_wire_str(op).ok_or_else(|| {
            JsValue::from_str("nostos write: invalid op (expected upsert|delete|patch)")
        })?;
        let normalized_payload = payload_json
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let write = PendingWrite {
            table: table.to_owned(),
            op: op_enum,
            pk: pk.to_owned(),
            payload_json: normalized_payload,
        };

        // Outbox: durable intent (enqueue) + instant local row (apply_local).
        // apply_local is best-effort by contract — an Err delays visibility to
        // the server echo, never loses the write (the entry is durable first).
        let mut engine = self.inner.engine.borrow_mut();
        let id = engine
            .storage_mut()
            .enqueue(write.clone())
            .map_err(|e| JsValue::from_str(&format!("nostos write: enqueue: {e}")))?;
        // apply_local renders the row instantly in the store (optimistic UI).
        let local_applied = engine.storage_mut().apply_local(&write).is_ok();
        // Release the engine borrow BEFORE the reactive tick + the ship path so
        // a callback that re-reads rows_for (and the mark_done re-borrow) can't
        // collide with this borrow_mut.
        drop(engine);

        // Ship now if OPEN; mark_done on success so the connected path drains
        // the outbox immediately. If the socket is NOT open, leave the write
        // pending for the onopen flush loop. Either way, return Ok — captured.
        if self.inner.ws.ready_state() == 1 && self.inner.ws.send_with_str(&frame).is_ok() {
            let _ = self.inner.engine.borrow_mut().storage_mut().mark_done(id);
        }
        // Reactive push (ADR-0024): a local write is a change tick too — node's
        // "remote apply OR local write" contract. apply_local already made the
        // row visible; fire the tick so a watcher sees its OWN write instantly,
        // without waiting for the server echo (which fires the remote-apply half
        // via the on_message pump). No-op when no callback is registered.
        if local_applied {
            transport::emit_change(&self.inner.on_change);
        }
        Ok(())
    }

    /// ADR-0029 D1: wipe the socket's engine rows + outbox (sign-out). Call
    /// before [`Self::close`]. Mirrors `nostos_client::SyncClient::clear_local_state`.
    #[wasm_bindgen(js_name = clearLocalState)]
    pub fn clear_local_state(&self) {
        self.inner.engine.borrow_mut().clear();
    }

    /// Close the socket. The server treats this as a session end; the client
    /// keeps its checkpoint so the next `connect` resumes.
    pub fn close(&self) {
        // Code 1000 = "normal closure". Errors here (e.g. already closed) are
        // ignorable — the socket is going away regardless.
        let _ = self.inner.ws.close_with_code(1000);
    }

    /// Register a reactive callback nostos invokes on EVERY change tick — the
    /// initial snapshot plus each delta — as the browser applies inbound WS
    /// frames. This is the Web port of node's `watch(onSnapshot)` / kotlin's
    /// `watch(SnapshotSink)` / Flutter's `watch(rows_sink)`: a TRUE Rust→JS push
    /// fired synchronously from the `onmessage` frame-pump on each commit, NOT a
    /// `setInterval` poll of `rowCount`.
    ///
    /// The callback receives NO args — it is a change *tick*. Read the fresh
    /// full-table snapshot via [`Self::rows_for`] inside the callback (the
    /// Worker host does exactly this, then forwards the rows to the main
    /// thread). This mirrors the engine's "full snapshot on every tick,
    /// self-healing on lag" contract (idempotent) and keeps the FFI boundary
    /// free of per-row marshalling.
    ///
    /// Registering replaces any prior callback (the old `Closure` is dropped →
    /// its JS function detached). The `Closure` is owned by the socket; call
    /// [`Self::off_change`] to stop, or simply drop the socket — no `.forget()`,
    /// so no leak (the one wasm-bindgen `Closure` pitfall).
    ///
    /// # Initial snapshot
    ///
    /// Fires the tick once on registration so a UI renders before the first WS
    /// frame commits. There is no subscribe-before-snapshot hazard here (the
    /// one the node/kotlin ports guard): the WASM pump IS the change tick, so a
    /// frame committed before registration is already in `rows_for`, and the
    /// initial tick's read sees it.
    ///
    /// JS:
    /// ```js
    /// sock.onChange(() => {
    ///   console.log("rows changed:", sock.rowsFor("tasks"));
    /// });
    /// ```
    #[wasm_bindgen(js_name = onChange)]
    pub fn on_change(&self, callback: js_sys::Function) {
        // Wrap the JS function in a no-arg Closure stored on the socket. The
        // on_message pump invokes it (via transport::emit_change) on every
        // commit; replacing a prior callback drops its Closure (detaches the JS
        // fn) — no leak.
        let cb = Closure::wrap(Box::new(move || {
            // Fire-and-forget: a JS error (the side tearing down) is swallowed;
            // the pump keeps running until the socket is dropped / off_change.
            let _ = callback.call0(&JsValue::UNDEFINED);
        }) as Box<dyn FnMut()>);
        *self.inner.on_change.borrow_mut() = Some(cb);
        // Initial tick: fire once on registration (full snapshot via rows_for
        // inside the callback).
        transport::emit_change(&self.inner.on_change);
    }

    /// Unregister the reactive callback. Drops the wrapped `Closure` (detaches
    /// the JS function). Idempotent. Dropping the socket also cleans up.
    #[wasm_bindgen(js_name = offChange)]
    pub fn off_change(&self) {
        *self.inner.on_change.borrow_mut() = None;
    }
}

use std::rc::Rc;
use wasm_bindgen::closure::Closure;

#[cfg(test)]
mod tests {
    use super::*;

    /// The `where_sql` field is the storage seam for the future WASM transport
    /// (E1): the engine holds the predicate so E1 can attach it to the subscribe
    /// frame. The apply path ignores it (the server filters upstream). These
    /// tests pin the getter/setter contract — the JS smoke test mirrors them.
    #[test]
    fn fresh_engine_has_no_where_sql() {
        let eng = NostosEngine::new();
        assert!(eng.where_sql.is_none());
    }

    #[test]
    fn set_where_sql_round_trips() {
        let mut eng = NostosEngine::new();
        eng.set_where_sql(Some("priority > 5".into()));
        assert_eq!(eng.where_sql(), Some("priority > 5".to_string()));
    }

    #[test]
    fn set_where_sql_none_clears() {
        let mut eng = NostosEngine::new();
        eng.set_where_sql(Some("priority > 5".into()));
        eng.set_where_sql(None);
        assert!(eng.where_sql.is_none());
    }

    #[test]
    fn set_where_sql_empty_string_is_treated_as_none() {
        // An empty predicate is a no-op (match-all); treat it as `None` so the
        // transport doesn't send `where_sql: ""` over the wire.
        let mut eng = NostosEngine::new();
        eng.set_where_sql(Some(String::new()));
        assert!(eng.where_sql.is_none());
    }

    // ---- rows_for: the readback the WASM FFI surfaces to JS ----
    //
    // These mirror the `InMemoryStorage::rows_for` host tests but through the
    // `NostosEngine` wrapper + `RowEntry` projection, so the JS-boundary types
    // (the `Vec<u8>` payload, the `RowEntry` shape) are pinned. The engine
    // feeds frames via its public `feed` (the same path JS takes), flushes, and
    // asserts the enumeration.

    fn feed_ins(eng: &mut NostosEngine, lsn: f64, table: &str, pk: &str, payload: &[u8]) {
        let frame = Frame::new(lsn, "insert", table, pk, Some(payload.to_vec()), None);
        // A standalone frame buffers; the outcome is None until flush.
        assert!(eng.feed(frame).unwrap().is_none());
    }

    #[test]
    fn rows_for_returns_flushed_rows_in_pk_order() {
        let mut eng = NostosEngine::new();
        // Insert out of pk order — the accessor hands back sorted.
        feed_ins(&mut eng, 10.0, "tasks", "2", b"bob");
        feed_ins(&mut eng, 20.0, "tasks", "1", b"alice");
        feed_ins(&mut eng, 30.0, "users", "9", b"carol"); // other table
        eng.flush().unwrap();

        let rows = eng.rows_for("tasks");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pk(), "1");
        assert_eq!(rows[0].payload(), b"alice");
        assert_eq!(rows[1].pk(), "2");
        assert_eq!(rows[1].payload(), b"bob");

        // A table with no rows yields an empty Vec.
        assert!(eng.rows_for("absent").is_empty());
    }

    #[test]
    fn clear_wipes_in_memory_rows() {
        // ADR-0029 D1: the sign-out wipe at the NostosEngine seam. The wipe
        // semantics are unit-tested in nostos-core; this proves the seam calls
        // Storage::clear (the outbox clear is the parallel trivial delegation).
        let mut eng = NostosEngine::new();
        feed_ins(&mut eng, 10.0, "tasks", "1", b"alice");
        feed_ins(&mut eng, 20.0, "tasks", "2", b"bob");
        eng.flush().unwrap();
        assert_eq!(eng.row_count(), 2, "seeded");
        eng.clear();
        assert_eq!(eng.row_count(), 0, "clear wiped rows");
        assert!(eng.rows_for("tasks").is_empty());
    }

    #[test]
    fn rows_for_empty_before_any_flush() {
        // Buffered-but-not-flushed frames are NOT yet in the store, so the
        // readback is empty until a commit lands.
        let mut eng = NostosEngine::new();
        feed_ins(&mut eng, 10.0, "tasks", "1", b"x");
        assert!(
            eng.rows_for("tasks").is_empty(),
            "buffered, not yet applied"
        );
        eng.flush().unwrap();
        assert_eq!(eng.rows_for("tasks").len(), 1);
    }

    #[test]
    fn rows_for_excludes_deleted_rows() {
        let mut eng = NostosEngine::new();
        feed_ins(&mut eng, 10.0, "tasks", "1", b"keep");
        feed_ins(&mut eng, 20.0, "tasks", "2", b"drop");
        eng.flush().unwrap();

        let del = Frame::new(30.0, "delete", "tasks", "2", None, None);
        eng.feed(del).unwrap();
        eng.flush().unwrap();

        let rows = eng.rows_for("tasks");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pk(), "1");
        assert_eq!(rows[0].payload(), b"keep");
    }
}

#[cfg(test)]
mod transport_tests {
    //! Host unit tests for the E1 transport's PURE layer. These run in `make ci`
    //! and are the real coverage (the browser WS glue is covered by the E3 demo
    //! page manual check — ponytail: browser wasm-bindgen-test setup is
    //! env-flaky; pure fns covered by host cargo tests; WS glue covered by E3).
    //!
    //! Tested here:
    //! - `decode_frames` (array + single-object + malformed + whitespace)
    //! - `decode_hex` (roundtrip, odd-length, non-hex)
    //! - `build_subscribe_frame` (with/without where_sql, with/without resume_lsn)
    //! - `build_ack_frame`
    //! - `on_message` pump (apply outcomes, ack LSN, batched arrays, deletes)
    //! - `checkpoint_key` + `parse_checkpoint`
    use super::*;
    use nostos_core::Operation;
    use transport::{
        build_ack_frame, build_subscribe_frame, build_write_frame, checkpoint_from, checkpoint_key,
        decode_frames, decode_hex, on_message, parse_checkpoint, pump_committed, PumpResult,
    };

    // ---- wire decode (mirror of nostos_infra::wire::decode_frames) ----

    fn frame_json(lsn: u64, op: &str, table: &str, pk: &str, payload_hex: Option<&str>) -> String {
        let payload = match payload_hex {
            Some(h) => format!(",\"payload\":\"{h}\""),
            None => String::new(),
        };
        format!(
            r#"{{"type":"event","lsn":{lsn},"op":"{op}","table":"{table}","pk":"{pk}"{payload}}}"#
        )
    }

    #[test]
    fn decode_single_object_frame() {
        let bytes = frame_json(10, "insert", "tasks", "1", Some("6869")); // "hi"
        let frames = decode_frames(bytes.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].lsn, 10);
        assert_eq!(frames[0].op, Operation::Insert);
        assert_eq!(frames[0].table, "tasks");
        assert_eq!(frames[0].pk, "1");
        assert_eq!(frames[0].payload.as_deref(), Some("6869"));
    }

    #[test]
    fn decode_array_of_frames_batched() {
        // C3 batched form: a JSON array of frames in one WS message.
        let arr = format!(
            "[{},{}]",
            frame_json(10, "insert", "tasks", "1", Some("6869")),
            frame_json(11, "update", "tasks", "2", Some("6f6b"))
        );
        let frames = decode_frames(arr.as_bytes());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].lsn, 10);
        assert_eq!(frames[1].lsn, 11);
        assert_eq!(frames[0].op, Operation::Insert);
        assert_eq!(frames[1].op, Operation::Update);
    }

    #[test]
    fn decode_delete_has_no_payload() {
        let bytes = frame_json(5, "delete", "tasks", "9", None);
        let frames = decode_frames(bytes.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].op, Operation::Delete);
        // payload absent on the wire → None.
        assert!(frames[0].payload.is_none());
    }

    #[test]
    fn decode_malformed_is_empty() {
        // Mirrors decode_frames' "drop malformed" contract.
        assert!(decode_frames(b"not json").is_empty());
        assert!(decode_frames(b"").is_empty());
        assert!(decode_frames(b"   ").is_empty());
        assert!(decode_frames(b"[\"not a frame\"]").is_empty());
    }

    #[test]
    fn decode_handles_leading_whitespace() {
        // The dispatch peeks the first NON-whitespace byte, so leading spaces
        // must not misroute an object into the array branch.
        let bytes = frame_json(7, "insert", "tasks", "1", Some("00"));
        let padded = format!("   {bytes}");
        let frames = decode_frames(padded.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].lsn, 7);
    }

    #[test]
    fn decode_empty_array_is_empty() {
        assert!(decode_frames(b"[]").is_empty());
    }

    #[test]
    fn wire_frame_op_lowercase_round_trips() {
        // The server emits lowercase op (Operation's serde rename_all); the
        // decode must accept exactly insert/update/delete.
        for op in ["insert", "update", "delete"] {
            let bytes = frame_json(1, op, "t", "1", None);
            let frames = decode_frames(bytes.as_bytes());
            assert_eq!(frames.len(), 1, "op={op} decoded");
        }
    }

    // ---- hex decode (mirror of nostos_client::decode_hex) ----

    #[test]
    fn decode_hex_round_trips() {
        assert_eq!(decode_hex("6869").as_deref(), Some(b"hi".as_slice()));
        assert_eq!(
            decode_hex("00ff10").as_deref(),
            Some(&[0x00, 0xff, 0x10][..])
        );
        assert_eq!(decode_hex("").as_deref(), Some(&[][..]));
    }

    #[test]
    fn decode_hex_odd_length_is_none() {
        assert_eq!(decode_hex("6"), None);
        assert_eq!(decode_hex("686"), None);
    }

    #[test]
    fn decode_hex_non_hex_is_none() {
        assert_eq!(decode_hex("6zzz"), None); // even length, bad chars
        assert_eq!(decode_hex("gg"), None);
    }

    // ---- subscribe frame builder (mirrors ClientMessage::Subscribe) ----

    #[test]
    fn subscribe_minimal_no_where_no_resume() {
        let json = build_subscribe_frame("tasks", None, None);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "subscribe");
        assert_eq!(v["table"], "tasks");
        assert_eq!(v["filters"], serde_json::json!([]));
        // resume_lsn + where_sql must be ABSENT (skip_serializing_if = None).
        assert!(v.get("resume_lsn").is_none());
        assert!(v.get("where_sql").is_none());
    }

    #[test]
    fn subscribe_with_where_sql_and_resume() {
        let json = build_subscribe_frame(
            "tasks",
            Some("status = 'open' AND priority >= 3"),
            Some(12_345),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "subscribe");
        assert_eq!(v["table"], "tasks");
        assert_eq!(v["filters"], serde_json::json!([]));
        assert_eq!(v["resume_lsn"], 12_345);
        assert_eq!(v["where_sql"], "status = 'open' AND priority >= 3");
    }

    #[test]
    fn subscribe_resume_only_no_where() {
        let json = build_subscribe_frame("users", None, Some(99));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["resume_lsn"], 99);
        assert!(v.get("where_sql").is_none(), "where_sql omitted when None");
    }

    #[test]
    fn subscribe_empty_where_sql_is_dropped() {
        // An empty predicate must NOT be sent (the server would reject "").
        let json = build_subscribe_frame("tasks", Some(""), Some(1));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("where_sql").is_none(), "empty where_sql dropped");
    }

    #[test]
    fn subscribe_decodes_back_as_clientmessage_shape() {
        // Round-trip: the JSON we build must be shape-compatible with the
        // server's decode_client_message. We mirror the field set here by
        // re-parsing into a Value and checking the tag.
        let json = build_subscribe_frame("tasks", Some("x > 1"), Some(5));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"].as_str(), Some("subscribe"));
        assert!(v["filters"].is_array());
    }

    // ---- ack frame builder ----

    #[test]
    fn ack_frame_shape() {
        let json = build_ack_frame(42);
        assert_eq!(json, r#"{"type":"ack","lsn":42}"#);
    }

    #[test]
    fn ack_frame_zero() {
        let json = build_ack_frame(0);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "ack");
        assert_eq!(v["lsn"], 0);
    }

    // ---- write frame builder ----

    #[test]
    fn write_upsert_frame_shape() {
        // Byte-for-byte the shape the spine's decode_client_message accepts:
        // type=write, payload is a JSON OBJECT, client_write_id echoed.
        let json = build_write_frame(
            "tasks",
            "upsert",
            "web-echo",
            Some(r#"{"title":"from-client","status":"open","priority":"5"}"#),
            "w1",
        )
        .expect("valid upsert frame");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "write");
        assert_eq!(v["table"], "tasks");
        assert_eq!(v["op"], "upsert");
        assert_eq!(v["pk"], "web-echo");
        assert_eq!(v["payload"]["title"], "from-client");
        assert_eq!(v["payload"]["status"], "open");
        assert_eq!(v["payload"]["priority"], "5");
        assert_eq!(v["client_write_id"], "w1");
    }

    #[test]
    fn write_delete_frame_omits_payload() {
        // Deletes carry no payload — skip_serializing_if = None.
        let json =
            build_write_frame("tasks", "delete", "stale", None, "w2").expect("valid delete frame");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "write");
        assert_eq!(v["op"], "delete");
        assert!(v.get("payload").is_none(), "payload absent on delete");
        assert_eq!(v["client_write_id"], "w2");
    }

    #[test]
    fn write_empty_payload_string_treated_as_delete() {
        // Empty / whitespace-only payload string → None (safe default, matches
        // the trim-and-filter guard in build_subscribe_frame).
        let json = build_write_frame("tasks", "upsert", "x", Some("   "), "w3")
            .expect("empty payload -> delete-shaped frame");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("payload").is_none());
    }

    // The error paths (non-object payload, malformed JSON) construct a
    // `JsValue` via `from_str`, which panics on a non-wasm host (JsValue is
    // browser-only). They're covered in the browser E2E
    // (`sdk/nostos_web/e2e/browser_live.spec.cjs`) — see ponytail on the
    // crate-level transport module for the testability split rationale.

    // ---- checkpoint key + parse ----

    #[test]
    fn checkpoint_key_format() {
        assert_eq!(checkpoint_key("tasks"), "cairn:checkpoint:tasks");
        assert_eq!(
            checkpoint_key("org_members"),
            "cairn:checkpoint:org_members"
        );
    }

    #[test]
    fn parse_checkpoint_valid() {
        assert_eq!(parse_checkpoint(Some("42")), Some(42));
        assert_eq!(parse_checkpoint(Some("  100  ")), Some(100)); // trimmed
        assert_eq!(parse_checkpoint(Some("0")), Some(0));
    }

    #[test]
    fn parse_checkpoint_missing_or_malformed() {
        assert_eq!(parse_checkpoint(None), None);
        assert_eq!(parse_checkpoint(Some("not a number")), None);
        assert_eq!(parse_checkpoint(Some("")), None);
        assert_eq!(parse_checkpoint(Some("12.5")), None); // not an integer
    }

    // ---- the pure frame-pump (on_message) ----

    #[test]
    fn pump_single_frame_buffers_until_flush_no_ack() {
        // A single standalone frame is buffered (no commit boundary) → no ack.
        let mut eng = NostosEngine::new();
        let bytes = frame_json(10, "insert", "tasks", "1", Some("6869"));
        let result = on_message(&mut eng, bytes.as_bytes()).unwrap();
        assert_eq!(result.applied, 1);
        assert_eq!(result.ack, None, "buffered frame → no commit → no ack");
        assert_eq!(eng.row_count(), 0, "not yet flushed");
    }

    #[test]
    fn pump_batched_frames_buffer_then_commit_on_flush() {
        // Two non-boundary frames in one message: the pump feeds both, but
        // neither triggers an in-message commit (no txn boundary, no soft
        // cap hit). They stay buffered; checkpoint is unchanged.
        let mut eng = NostosEngine::new();
        let batch = format!(
            "[{},{}]",
            frame_json(10, "insert", "tasks", "a", Some("00")),
            frame_json(11, "insert", "tasks", "b", Some("00"))
        );
        let _ = on_message(&mut eng, batch.as_bytes()).unwrap();
        // Nothing committed yet (both buffered, no boundary in this message).
        assert_eq!(eng.checkpoint() as u64, 0);

        // Flush via the engine directly (the WS glue does this on close/idle).
        let outcome = eng.flush().unwrap().expect("had pending");
        assert_eq!(outcome.checkpoint() as u64, 11);
        assert_eq!(eng.row_count(), 2);
    }

    #[test]
    fn pump_batched_array_applies_all_frames() {
        // A C3 batched array: 3 frames in one message. With the default soft
        // cap (256), none commit in-message; we flush + assert all applied.
        let mut eng = NostosEngine::new();
        let batch = format!(
            "[{},{},{}]",
            frame_json(10, "insert", "tasks", "1", Some("6869")),
            frame_json(20, "insert", "tasks", "2", Some("6f6b")),
            frame_json(30, "insert", "tasks", "3", Some("00")),
        );
        let result = on_message(&mut eng, batch.as_bytes()).unwrap();
        assert_eq!(result.applied, 3);
        let outcome = eng.flush().unwrap().expect("had pending");
        assert_eq!(outcome.checkpoint() as u64, 30);
        assert_eq!(eng.row_count(), 3);
    }

    #[test]
    fn pump_delete_payload_decodes_to_none() {
        // A delete carries no payload; the pump must hex-decode None → None and
        // the engine removes the row (idempotent on absent row).
        let mut eng = NostosEngine::new();
        // Seed a row first.
        let seed = frame_json(10, "insert", "tasks", "1", Some("6869"));
        let _ = on_message(&mut eng, seed.as_bytes()).unwrap();
        eng.flush().unwrap();
        assert_eq!(eng.row_count(), 1);

        // Delete it.
        let del = frame_json(20, "delete", "tasks", "1", None);
        let _ = on_message(&mut eng, del.as_bytes()).unwrap();
        eng.flush().unwrap();
        assert_eq!(eng.row_count(), 0, "delete removed the row");
        assert_eq!(eng.checkpoint() as u64, 20);
    }

    #[test]
    fn pump_malformed_message_is_no_op() {
        // Garbage bytes → decode_frames returns [] → applied=0, no ack, no panic.
        let mut eng = NostosEngine::new();
        let result = on_message(&mut eng, b"totally not json").unwrap();
        assert_eq!(
            result,
            PumpResult {
                applied: 0,
                ack: None
            }
        );
    }

    #[test]
    fn pump_checkpoint_from_helper() {
        // checkpoint_from(result) == result.ack — the named accessor for the
        // localStorage-write value.
        let result = PumpResult {
            applied: 2,
            ack: Some(50),
        };
        assert_eq!(checkpoint_from(result), Some(50));
        let result = PumpResult {
            applied: 2,
            ack: None,
        };
        assert_eq!(checkpoint_from(result), None);
    }

    #[test]
    fn pump_hex_payload_decodes_before_apply() {
        // The wire payload is hex; the pump hex-decodes it once. b"hi" == "6869".
        // We can't read the decoded bytes back through the engine (opaque), but
        // we assert the apply succeeded with the right row count — a decode
        // failure (None payload on an insert) would still apply (empty payload),
        // so this is a smoke that the path doesn't panic on valid hex.
        let mut eng = NostosEngine::new();
        let bytes = frame_json(10, "insert", "tasks", "1", Some("6869"));
        let result = on_message(&mut eng, bytes.as_bytes()).unwrap();
        assert_eq!(result.applied, 1);
        eng.flush().unwrap();
        assert_eq!(eng.row_count(), 1);
    }

    #[test]
    fn pump_replay_is_idempotent_through_resume() {
        // The resume contract: after a flush at LSN 20, re-feeding frames ≤ 20
        // must not duplicate rows (idempotent upsert-by-pk). This is the
        // localStorage-checkpoint + replay ceiling.
        let mut eng = NostosEngine::new();

        // Apply frames 10 + 20.
        let batch = format!(
            "[{},{}]",
            frame_json(10, "insert", "tasks", "1", Some("6869")),
            frame_json(20, "insert", "tasks", "2", Some("6f6b")),
        );
        let _ = on_message(&mut eng, batch.as_bytes()).unwrap();
        eng.flush().unwrap();
        assert_eq!(eng.checkpoint() as u64, 20);
        assert_eq!(eng.row_count(), 2);

        // "Replay" the same frames (idempotent).
        let _ = on_message(&mut eng, batch.as_bytes()).unwrap();
        eng.flush().unwrap();
        assert_eq!(eng.row_count(), 2, "replay did not duplicate");
    }

    // ---- reactive push trigger (ADR-0024 — the Web watch() port) ----
    //
    // The WASM reactive primitive is "fire the snapshot callback iff this
    // message committed a frame." The decision is pure (`pump_committed`); the
    // `Closure`/JS wiring is browser-only (`JsValue` panics on a host — same
    // split as the rest of the WS glue, and the same reason node's
    // `watch_internal` takes a `SnapshotEmitter` seam: a `ThreadsafeFunction` /
    // `Closure` cannot be built without a live JS runtime). These pin the
    // trigger so the push can't silently regress to firing on no-ops (a spurious
    // snapshot) or staying mute on commits (a missed delta).

    #[test]
    fn pump_committed_true_when_pump_acked_in_message() {
        // A transaction boundary commits mid-message → pump.ack is Some → tick.
        let pump = PumpResult {
            applied: 1,
            ack: Some(10),
        };
        assert!(pump_committed(&pump, false));
    }

    #[test]
    fn pump_committed_true_when_only_trailing_flush_committed() {
        // A buffered standalone frame: the pump itself didn't commit (ack None),
        // but the trailing idle-flush did → still a change tick. This is the
        // common browser path (one Nostos event per WS message → flushed at
        // message end).
        let pump = PumpResult {
            applied: 1,
            ack: None,
        };
        assert!(pump_committed(&pump, true));
    }

    #[test]
    fn pump_committed_false_when_nothing_committed() {
        // A malformed / no-op message: nothing applied, nothing committed → no
        // tick (no spurious snapshot).
        let pump = PumpResult {
            applied: 0,
            ack: None,
        };
        assert!(!pump_committed(&pump, false));
    }

    #[test]
    fn reactive_snapshot_after_commit_is_the_full_table() {
        // The push's payload is `rows_for(table)` read at the tick. Prove the
        // commit → trigger → full-snapshot chain on the pure core (the part that
        // can't touch JsValue): a buffered frame does NOT tick until flush, then
        // does, and the snapshot the callback would forward is the full table
        // (pk-sorted, the same shape `rows_for` pins).
        let mut eng = NostosEngine::new();
        let bytes = frame_json(10, "insert", "tasks", "1", Some("6869"));
        let pump = on_message(&mut eng, bytes.as_bytes()).unwrap();
        assert!(
            !pump_committed(&pump, false),
            "buffered frame → no in-message commit → no tick"
        );
        assert_eq!(eng.row_count(), 0, "not yet flushed → empty snapshot");

        let flush_committed = eng.flush().unwrap().is_some();
        assert!(flush_committed);
        assert!(
            pump_committed(&pump, flush_committed),
            "flush committed → tick fires"
        );

        // The snapshot delivered at the tick: full current table.
        let snapshot = eng.rows_for("tasks");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].pk(), "1");
    }
}
