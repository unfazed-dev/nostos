//! # nostos-ffi-wasm — the WebAssembly bridge over `nostos-core`.
//!
//! A thin `#[wasm_bindgen]` projection of the apply engine for JavaScript.
//! The engine, atomic batching, and idempotency all live in `nostos-core` (pure
//! Rust, tested by 18 unit tests); this crate only adapts the public surface to
//! JS-friendly types. No new logic.
//!
//! ## Scope (ADR-0015)
//!
//! This first slice exposes the **in-memory apply path**: build an engine, feed
//! frames, flush, read the checkpoint + row count. It proves the WASM bundle
//! stays under budget (ADR-0015's kill criterion) and that the JS↔Rust boundary
//! works end-to-end.
//!
//! What's NOT here (ponytail — deferred):
//! - **OPFS persistence** — the browser-durable backend needs a Web Worker +
//!   sync-OPFS plumbing (Worker-only by spec); a verified follow-up.
//! - **The transport** — `SyncClient` (tokio) doesn't run on wasm; a
//!   `web-sys` WebSocket transport is a separate slice.
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

use nostos_core::{ApplyEngine, ApplyOutcome, Frame as CoreFrame, InMemoryStorage, Lsn, Operation};
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
    /// reload — real browser persistence (OPFS) is a deferred follow-up.
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
}

impl Default for NostosEngine {
    fn default() -> Self {
        Self::new()
    }
}

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
}
