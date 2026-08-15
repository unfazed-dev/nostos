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
    PendingWrite, RowOp, Storage, WriteOp,
};
use std::cell::Cell;
use std::collections::HashSet;
use wasm_bindgen::prelude::*;

// Wave 4a: `js_sys::Reflect` for reading JS object fields in typed-verb
// orchestration (writeBatch, applySchema parse JS arrays of objects).
use js_sys::Reflect;

/// The SQLite-WASM durable backend (ADR-0017 follow-up / ADR-0033).
pub mod sqlite_wasm;
pub use sqlite_wasm::SqliteWasmStorage;

/// The unified storage backend for the WASM bridge — either in-memory (the
/// node smoke + standalone default) or SQLite-WASM/OPFS (the Worker's durable
/// path). Both implement [`Storage`] + [`Outbox`]; the enum delegates. This lets
/// [`NostosEngine`] / [`NostosSocket`] work with either backend without generics
/// crossing the wasm-bindgen boundary (generics can't be `#[wasm_bindgen]`).
///
/// The durable variant is browser-only (the JS sqlite-wasm methods exist only
/// in a Worker with OPFS). Host cargo tests exercise only `Memory`; the
/// `SqliteWasm` path is proven by the Playwright browser test (ADR-0033).
pub(crate) enum WebStorage {
    /// RAM-only — the node smoke / standalone default + the OPFS-unavailable
    /// degrade path (ADR-0017 follow-up scope step 5 / ADR-0033).
    Memory(InMemoryStorage),
    /// OPFS-backed SQLite-WASM — durable across reloads. The Worker creates this
    /// after async sqlite-wasm init + `opfs-sahpool`.
    SqliteWasm(SqliteWasmStorage),
}

impl Storage for WebStorage {
    fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        match self {
            WebStorage::Memory(s) => s.checkpoint(),
            WebStorage::SqliteWasm(s) => s.checkpoint(),
        }
    }
    fn epoch(&self) -> nostos_core::Result<u64> {
        match self {
            WebStorage::Memory(s) => s.epoch(),
            WebStorage::SqliteWasm(s) => s.epoch(),
        }
    }
    fn save_epoch(&self, epoch: u64) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.save_epoch(epoch),
            WebStorage::SqliteWasm(s) => s.save_epoch(epoch),
        }
    }
    fn apply_batch(
        &mut self,
        ops: &[(RowOp, u64)],
        checkpoint: Lsn,
        snapshot_tables: &HashSet<String>,
    ) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.apply_batch(ops, checkpoint, snapshot_tables),
            WebStorage::SqliteWasm(s) => s.apply_batch(ops, checkpoint, snapshot_tables),
        }
    }
    fn pks_for_table(&self, table: &str) -> nostos_core::Result<Vec<String>> {
        match self {
            WebStorage::Memory(s) => s.pks_for_table(table),
            WebStorage::SqliteWasm(s) => s.pks_for_table(table),
        }
    }
    fn delete_pks(&mut self, table: &str, pks: &[String]) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.delete_pks(table, pks),
            WebStorage::SqliteWasm(s) => s.delete_pks(table, pks),
        }
    }
    fn clear(&mut self) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => Storage::clear(s),
            WebStorage::SqliteWasm(s) => Storage::clear(s),
        }
    }

    /// Wave 4a: delegate `read_payload` so counter RMW works on BOTH backends.
    /// `InMemoryStorage` overrides it; `SqliteWasmStorage` overrides it (above).
    /// Without this delegation, the trait default (`Ok(None)`) would shadow
    /// both real impls via `WebStorage`.
    fn read_payload(&self, table: &str, pk: &str) -> nostos_core::Result<Option<Vec<u8>>> {
        match self {
            WebStorage::Memory(s) => s.read_payload(table, pk),
            WebStorage::SqliteWasm(s) => s.read_payload(table, pk),
        }
    }
}

impl Outbox for WebStorage {
    fn enqueue(&mut self, write: PendingWrite) -> nostos_core::Result<u64> {
        match self {
            WebStorage::Memory(s) => s.enqueue(write),
            WebStorage::SqliteWasm(s) => s.enqueue(write),
        }
    }
    fn pending(&self) -> nostos_core::Result<Vec<(u64, PendingWrite)>> {
        match self {
            WebStorage::Memory(s) => s.pending(),
            WebStorage::SqliteWasm(s) => s.pending(),
        }
    }
    fn mark_done(&mut self, id: u64) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.mark_done(id),
            WebStorage::SqliteWasm(s) => s.mark_done(id),
        }
    }
    fn bump_attempts(&self, id: u64) -> nostos_core::Result<u32> {
        match self {
            WebStorage::Memory(s) => s.bump_attempts(id),
            WebStorage::SqliteWasm(s) => s.bump_attempts(id),
        }
    }
    fn mark_dead_letter(&self, id: u64) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.mark_dead_letter(id),
            WebStorage::SqliteWasm(s) => s.mark_dead_letter(id),
        }
    }
    /// Wave 4a: override to persist `last_error` + `dead_lettered_at` (ADR-0032
    /// T5). Delegates to whichever backend overrides it.
    fn mark_dead_letter_with_error(&self, id: u64, error: Option<&str>) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.mark_dead_letter_with_error(id, error),
            WebStorage::SqliteWasm(s) => s.mark_dead_letter_with_error(id, error),
        }
    }
    /// Wave 4a: transactional batch enqueue (ADR-0032 T3). `InMemoryStorage`
    /// overrides with an atomic BTreeMap extend; `SqliteWasmStorage` overrides
    /// with BEGIN → loop → COMMIT. Without this delegation, the trait default
    /// (sequential `enqueue` loop) would shadow both — non-atomic.
    fn enqueue_batch(&mut self, writes: Vec<PendingWrite>) -> nostos_core::Result<Vec<u64>> {
        match self {
            WebStorage::Memory(s) => s.enqueue_batch(writes),
            WebStorage::SqliteWasm(s) => s.enqueue_batch(writes),
        }
    }
    fn apply_local(&mut self, write: &PendingWrite) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => s.apply_local(write),
            WebStorage::SqliteWasm(s) => s.apply_local(write),
        }
    }
    fn clear(&mut self) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(s) => Outbox::clear(s),
            WebStorage::SqliteWasm(s) => Outbox::clear(s),
        }
    }
}

impl WebStorage {
    /// Row count (diagnostics — NOT on the `Storage` trait). Delegates to
    /// whichever backend is active. The SqliteWasm variant queries `COUNT(*)`.
    pub(crate) fn row_count(&self) -> usize {
        match self {
            WebStorage::Memory(s) => s.row_count(),
            WebStorage::SqliteWasm(s) => s.row_count(),
        }
    }

    /// `(pk, payload)` pairs for `table` sorted by pk (diagnostics readback).
    /// Delegates to whichever backend is active.
    pub(crate) fn rows_for(&self, table: &str) -> Vec<(String, Vec<u8>)> {
        match self {
            WebStorage::Memory(s) => s.rows_for(table),
            WebStorage::SqliteWasm(s) => s.rows_for(table),
        }
    }

    /// Whether this backend is the durable (SqliteWasm) variant. Used by the
    /// transport to decide whether to read the checkpoint from SQLite (durable)
    /// or `localStorage` (in-memory degrade path).
    pub(crate) fn is_durable(&self) -> bool {
        matches!(self, WebStorage::SqliteWasm(_))
    }

    // ---- Wave 4a: typed-verb support (CRDT tables, read/query, status) ----

    /// Tag tables as OR-set / counter CRDTs. Propagates to whichever backend
    /// is active. Mirrors `SqliteStorage::with_or_set_tables` /
    /// `with_counter_tables` and `InMemoryStorage`'s same builders.
    pub(crate) fn set_crdt_tables(&mut self, or_set: HashSet<String>, counter: HashSet<String>) {
        match self {
            WebStorage::Memory(s) => {
                s.set_or_set_tables(or_set);
                s.set_counter_tables(counter);
            }
            WebStorage::SqliteWasm(s) => {
                s.set_or_set_tables(or_set);
                s.set_counter_tables(counter);
            }
        }
    }

    /// Read the raw payload bytes for `(table, pk)`, or `None`. Wraps the
    /// `Storage::read_payload` trait delegation.
    pub(crate) fn read_payload(
        &self,
        table: &str,
        pk: &str,
    ) -> nostos_core::Result<Option<Vec<u8>>> {
        Storage::read_payload(self, table, pk)
    }

    /// Transactional batch enqueue. Wraps the `Outbox::enqueue_batch` trait
    /// delegation (overridden on both backends for atomicity).
    pub(crate) fn enqueue_batch(
        &mut self,
        writes: Vec<PendingWrite>,
    ) -> nostos_core::Result<Vec<u64>> {
        Outbox::enqueue_batch(self, writes)
    }

    /// Run an arbitrary SELECT, returning JSON. SqliteWasm only (Memory has no
    /// SQL engine). Returns `[]"` on Memory (no error — the dev shouldn't call
    /// query on an in-memory engine; it's a diagnostics convenience).
    pub(crate) fn query_json(&self, sql: &str) -> nostos_core::Result<String> {
        match self {
            WebStorage::Memory(_) => Ok("[]".to_string()),
            WebStorage::SqliteWasm(s) => s
                .query_json(sql)
                .map_err(|e| nostos_core::StorageError::Backend(e.to_string())),
        }
    }

    /// Materialize WS2 read-views. SqliteWasm only (Memory has no views).
    /// No-op on Memory (rows are already accessible by table).
    pub(crate) fn apply_schema(&self, tables: &[(String, Vec<String>)]) -> nostos_core::Result<()> {
        match self {
            WebStorage::Memory(_) => Ok(()),
            WebStorage::SqliteWasm(s) => s
                .apply_schema(tables)
                .map_err(|e| nostos_core::StorageError::Backend(e.to_string())),
        }
    }

    /// Pending (non-dead-lettered) write count.
    pub(crate) fn pending_count(&self) -> u64 {
        match self {
            WebStorage::Memory(s) => s.pending().map_or(0, |p| p.len() as u64),
            WebStorage::SqliteWasm(s) => s.pending_count(),
        }
    }

    /// Dead-lettered write count.
    pub(crate) fn dead_letter_count(&self) -> u64 {
        match self {
            WebStorage::Memory(_) => 0, // InMemoryStorage has no dead-letter column
            WebStorage::SqliteWasm(s) => s.dead_letter_count(),
        }
    }

    /// The last error from the most recent dead-lettered write.
    pub(crate) fn last_dead_letter_error(&self) -> Option<String> {
        match self {
            WebStorage::Memory(_) => None,
            WebStorage::SqliteWasm(s) => s.last_dead_letter_error(),
        }
    }
}

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

/// Derive a stable per-engine replica id. Uses wall-clock ms + a process-local
/// counter so two engines constructed in the same ms get distinct ids. Mirrors
/// `SyncClientConfig::client_id` derivation. Called once at construction.
/// ponytail: a proper UUID would be more collision-resistant but adds a dep;
/// the ms-precision timestamp is sufficient for wasm (single-threaded, one
/// engine per Worker).
fn derive_replica_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("wasm-{now}-{seq}")
}

/// Wall-clock time in milliseconds since the Unix epoch. On wasm, uses
/// `js_sys::Date::now()` (real browser clock); on host (cargo test), uses
/// `SystemTime` (the host has a real clock). This cfg split keeps host unit
/// tests panic-free while giving wasm real timestamps for HLC minting.
fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// Read a string field from a JS object. Returns `None` if the field is absent
/// or not a string. Used by typed-verb parsers (writeBatch, applySchema).
fn js_get_str(obj: &js_sys::Object, key: &str) -> Option<String> {
    let val = Reflect::get(obj, &JsValue::from_str(key)).ok()?;
    if val.is_string() {
        val.as_string()
    } else {
        None
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
    inner: ApplyEngine<WebStorage>,
    /// The optional safe-SQL predicate for the next subscribe. Held here so the
    /// future WASM transport (E1) can read it when sending the subscribe frame;
    /// the in-memory apply path ignores it (the server filters upstream).
    where_sql: Option<String>,
    /// Stable per-engine replica id for PN-Counter CRDT (ADR-0030 addendum).
    /// Derived once at construction. Mirrors `SyncClientConfig::client_id`.
    replica_id: String,
    /// Client HLC state for optimistic OR-set edits (ADR-0030 Decision 4,
    /// relaxed): each `or_set_add` / `or_set_remove` mints the next HLC here so
    /// a local edit is comparable to remote elements on merge. `Cell` (not
    /// `Mutex`) — wasm is single-threaded, no lock needed. `None` until the
    /// first mint. Mirrors native `SyncClient::hlc_state` (client.rs L314).
    hlc_state: Cell<Option<nostos_domain::Hlc>>,
}

#[wasm_bindgen]
impl NostosEngine {
    /// Create an in-memory engine. Data survives the apply loop but NOT a page
    /// reload — durable browser persistence (SQLite-WASM/OPFS) is the Worker's
    /// durable path (ADR-0017 follow-up / ADR-0033). This is the node-smoke +
    /// standalone default + the OPFS-unavailable degrade path.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: ApplyEngine::new(WebStorage::Memory(InMemoryStorage::new())),
            where_sql: None,
            replica_id: derive_replica_id(),
            hlc_state: Cell::new(None),
        }
    }

    /// Create an engine backed by a durable SQLite-WASM/OPFS store. Called by
    /// the Worker (via [`NostosSocket::connect`]'s `db_handle` arg) after async
    /// sqlite-wasm init. The JS `db` is a wrapper around the sqlite-wasm
    /// instance (see `sdk/nostos_web/worker/sqlite_wasm_glue.js`). Browser-only.
    #[doc(hidden)]
    pub(crate) fn with_durable(db: js_sys::Object) -> Self {
        Self {
            inner: ApplyEngine::new(WebStorage::SqliteWasm(SqliteWasmStorage::new(db))),
            where_sql: None,
            replica_id: derive_replica_id(),
            hlc_state: Cell::new(None),
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

    /// Mutable access to the backing `WebStorage` — the outbox flush path
    /// (`Outbox::enqueue` / `apply_local` / `pending` / `mark_done`) reaches the
    /// store through this. `pub(crate)` because the JS surface never mutates
    /// storage directly; only [`NostosSocket`]'s write/flush path (WS1) does.
    pub(crate) fn storage_mut(&mut self) -> &mut WebStorage {
        self.inner.storage_mut()
    }

    /// Read-only access to the backing `WebStorage` — used by the transport to
    /// check `is_durable()` for the checkpoint-reading decision (ADR-0033).
    pub(crate) fn storage(&self) -> &WebStorage {
        self.inner.storage()
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
        // `Storage::clear` on `WebStorage::SqliteWasm` runs `clearAll()`
        // (DELETE rows + outbox + reset checkpoint); `Outbox::clear` is the
        // outbox-only half (redundant after Storage::clear, but correct).
        let _ = <WebStorage as Storage>::clear(s);
        let _ = <WebStorage as Outbox>::clear(s);
    }

    // ========================================================================
    // Wave 4a: the typed Tier-1 surface (ADR-0032 T1–T5).
    // ========================================================================
    //
    // These are the wasm counterpart of the native `SyncClient` typed verbs
    // (writeBatch, orSetAdd/Remove, counterIncrement/Decrement, applySchema,
    // query, watchWriteStatus). They port only the thin *orchestration*
    // (read-modify-write → enqueue → apply_local); all CRDT invariants live in
    // `nostos-domain` (reused, NOT re-implemented). The native `SyncClient` is
    // NOT touched — it is tokio-based and native-only, unreachable from wasm.

    /// Configure which tables are OR-set / counter CRDTs. Call BEFORE any
    /// orSet/counter verb — the loud-fail gate checks the tag before minting.
    /// Mirrors `SyncClientConfig::or_set_tables` / `counter_tables`.
    #[wasm_bindgen(js_name = setCrdtTables)]
    pub fn set_crdt_tables(&mut self, or_set: Vec<String>, counter: Vec<String>) {
        let or_set_set: HashSet<String> = or_set.into_iter().collect();
        let counter_set: HashSet<String> = counter.into_iter().collect();
        self.inner
            .storage_mut()
            .set_crdt_tables(or_set_set, counter_set);
    }

    /// Enqueue a batch of writes atomically (ADR-0032 T3). All ops commit in
    /// one SQLite txn (SqliteWasm) or one BTreeMap extend (Memory) — a
    /// mid-batch failure rolls back the entire batch. Returns the outbox ids
    /// in order. Each op is also `apply_local`'d for instant optimistic UI.
    ///
    /// JS: `eng.writeBatch([{table, op, pk, payloadJson?}, ...])` → `[id1, id2, …]`
    #[wasm_bindgen(js_name = writeBatch)]
    #[allow(clippy::needless_pass_by_value)] // wasm_bindgen requires owned Vec<JsValue> (no RefFromWasmAbi for [JsValue])
    pub fn write_batch(&mut self, ops: Vec<JsValue>) -> Result<Vec<f64>, JsValue> {
        let mut writes = Vec::with_capacity(ops.len());
        for op in &ops {
            let obj = js_sys::Object::from(op.clone());
            let table = js_get_str(&obj, "table")
                .ok_or_else(|| JsValue::from_str("writeBatch: missing table"))?;
            let op_str = js_get_str(&obj, "op")
                .ok_or_else(|| JsValue::from_str("writeBatch: missing op"))?;
            let pk = js_get_str(&obj, "pk")
                .ok_or_else(|| JsValue::from_str("writeBatch: missing pk"))?;
            let payload_json = js_get_str(&obj, "payloadJson");
            let op_enum = WriteOp::from_wire_str(&op_str).ok_or_else(|| {
                JsValue::from_str(&format!(
                    "writeBatch: invalid op '{op_str}' (expected upsert|delete|patch)"
                ))
            })?;
            writes.push(PendingWrite {
                table,
                op: op_enum,
                pk,
                payload_json,
            });
        }
        let s = self.inner.storage_mut();
        let ids = s
            .enqueue_batch(writes.clone())
            .map_err(|e| JsValue::from_str(&format!("writeBatch: enqueue: {e}")))?;
        // apply_local each write for optimistic UI (best-effort).
        for w in &writes {
            let _ = s.apply_local(w);
        }
        Ok(ids.into_iter().map(|id| id as f64).collect())
    }

    /// Add `element` to the add-wins OR-set in row `pk` of `table` (ADR-0030 /
    /// ADR-0032 T4). Mints a client HLC and enqueues a merge-upsert. The
    /// element renders locally immediately and converges with concurrent
    /// remote adds on the server's echo.
    ///
    /// ponytail: mirrors SyncClient::or_set_add (client.rs L571); rewire to
    /// share when convenient.
    #[wasm_bindgen(js_name = orSetAdd)]
    pub fn or_set_add(&mut self, table: &str, pk: &str, element: &str) -> Result<f64, JsValue> {
        self.or_set_op(table, pk, element, false)
    }

    /// Remove `element` from the OR-set — a tombstone at a fresh HLC. Add-wins:
    /// a concurrent or later re-add (a higher HLC) re-activates the element.
    #[wasm_bindgen(js_name = orSetRemove)]
    pub fn or_set_remove(&mut self, table: &str, pk: &str, element: &str) -> Result<f64, JsValue> {
        self.or_set_op(table, pk, element, true)
    }

    /// Shared OR-set add/remove: mint HLC, build OrSetPayload, enqueue upsert,
    /// apply_local. Mirrors SyncClient::or_set_op (client.rs L594).
    fn or_set_op(
        &mut self,
        table: &str,
        pk: &str,
        element: &str,
        remove: bool,
    ) -> Result<f64, JsValue> {
        let h = self.mint_hlc();
        let element_struct = nostos_domain::OrSetElement {
            v: element.to_string(),
            h: if remove { nostos_domain::Hlc::ZERO } else { h },
            d: if remove { Some(h) } else { None },
        };
        let payload = serde_json::to_string(&nostos_domain::OrSetPayload {
            elements: vec![element_struct],
        })
        .expect("OrSetPayload serializes infallibly");
        let write = PendingWrite {
            table: table.to_string(),
            op: WriteOp::Upsert,
            pk: pk.to_string(),
            payload_json: Some(payload),
        };
        let s = self.inner.storage_mut();
        let id = s
            .enqueue(write.clone())
            .map_err(|e| JsValue::from_str(&format!("orSetOp: enqueue: {e}")))?;
        let _ = s.apply_local(&write);
        Ok(id as f64)
    }

    /// Increment the PN-Counter in row `pk` of `table` by `delta` (ADR-0030
    /// addendum / ADR-0032 T4). Read-modify-write: reads the current counter
    /// payload, applies the delta to this replica's entry, and enqueues the
    /// result.
    ///
    /// ponytail: mirrors SyncClient::counter_op (client.rs L665); rewire to
    /// share when convenient.
    #[wasm_bindgen(js_name = counterIncrement)]
    pub fn counter_increment(&mut self, table: &str, pk: &str, delta: f64) -> Result<f64, JsValue> {
        self.counter_op(table, pk, delta as i64)
    }

    /// Decrement the PN-Counter by `delta` (bumps the negative counter `n`).
    #[wasm_bindgen(js_name = counterDecrement)]
    pub fn counter_decrement(&mut self, table: &str, pk: &str, delta: f64) -> Result<f64, JsValue> {
        let neg = -(delta as i64);
        self.counter_op(table, pk, neg)
    }

    /// Shared counter RMW: read existing payload → apply delta → enqueue upsert.
    /// Wasm is single-threaded — no lock needed (unlike native's engine lock).
    fn counter_op(&mut self, table: &str, pk: &str, delta: i64) -> Result<f64, JsValue> {
        let s = self.inner.storage_mut();
        let existing = s
            .read_payload(table, pk)
            .map_err(|e| JsValue::from_str(&format!("counter: read_payload: {e}")))?
            .unwrap_or_default();
        let payload_bytes = nostos_domain::counter_apply_delta(&existing, &self.replica_id, delta);
        let payload_json = String::from_utf8(payload_bytes)
            .expect("counter_apply_delta serializes valid UTF-8 JSON");
        let write = PendingWrite {
            table: table.to_string(),
            op: WriteOp::Upsert,
            pk: pk.to_string(),
            payload_json: Some(payload_json),
        };
        let id = s
            .enqueue(write.clone())
            .map_err(|e| JsValue::from_str(&format!("counter: enqueue: {e}")))?;
        let _ = s.apply_local(&write);
        Ok(id as f64)
    }

    /// Materialize the WS2 read-views over `cairn_data`. After this,
    /// `SELECT col FROM <table>` resolves against a VIEW that
    /// `json_extract`s each column from the opaque payload. SqliteWasm only
    /// (Memory is a no-op). Mirrors native `SqliteStorage::apply_schema`.
    ///
    /// JS: `eng.applySchema([{name, columns}, ...])`
    #[wasm_bindgen(js_name = applySchema)]
    #[allow(clippy::needless_pass_by_value)] // wasm_bindgen requires owned Vec<JsValue>
    pub fn apply_schema(&mut self, tables: Vec<JsValue>) -> Result<(), JsValue> {
        let mut mapped = Vec::with_capacity(tables.len());
        for t in &tables {
            let obj = js_sys::Object::from(t.clone());
            let name = js_get_str(&obj, "name")
                .ok_or_else(|| JsValue::from_str("applySchema: missing name"))?;
            let columns_val = Reflect::get(&obj, &"columns".into())
                .map_err(|_| JsValue::from_str("applySchema: missing columns"))?;
            let columns_arr = js_sys::Array::from(&columns_val);
            let mut cols = Vec::new();
            for i in 0..columns_arr.length() {
                if let Some(s) = columns_arr.get(i).as_string() {
                    cols.push(s);
                }
            }
            mapped.push((name, cols));
        }
        self.inner
            .storage()
            .apply_schema(&mapped)
            .map_err(|e| JsValue::from_str(&format!("applySchema: {e}")))
    }

    /// Run an arbitrary SELECT, returning a JSON-array-of-objects string.
    /// SqliteWasm only (Memory returns `"[]"`). Mirrors native
    /// `SqliteStorage::query` (sqlite.rs L416).
    #[wasm_bindgen(js_name = query)]
    pub fn query(&self, sql: &str) -> Result<String, JsValue> {
        self.inner
            .storage()
            .query_json(sql)
            .map_err(|e| JsValue::from_str(&format!("query: {e}")))
    }

    /// The current pending (non-dead-lettered) write count. For
    /// `watchWriteStatus`.
    #[wasm_bindgen(getter, js_name = pendingCount)]
    pub fn pending_count(&self) -> u32 {
        self.inner.storage().pending_count() as u32
    }

    /// The current dead-lettered write count. For `watchWriteStatus`.
    #[wasm_bindgen(getter, js_name = deadLetteredCount)]
    pub fn dead_lettered_count(&self) -> u32 {
        self.inner.storage().dead_letter_count() as u32
    }

    /// The last error from the most recent dead-lettered write (or null).
    #[wasm_bindgen(getter, js_name = lastError)]
    pub fn last_error(&self) -> Option<String> {
        self.inner.storage().last_dead_letter_error()
    }

    /// Mint the next client HLC (ADR-0030 Decision 4). Uses `js_sys::Date::now`
    /// for wall-clock time (seconds since epoch as f64; multiply by 1000 for
    /// ms). The logical counter preserves monotonicity if the clock jumps back.
    /// Mirrors SyncClient::mint_hlc (client.rs L731).
    fn mint_hlc(&self) -> nostos_domain::Hlc {
        let now_wall_ms = now_ms();
        let prev = self.hlc_state.get();
        let h = nostos_domain::Hlc::mint(prev, now_wall_ms);
        self.hlc_state.set(Some(h));
        h
    }

    /// Read the raw payload bytes for `(table, pk)`. Used by host tests to
    /// assert CRDT-merged state. Not exposed to JS (the counter_value helper
    /// in the JS wrapper parses payloads from `query` results instead).
    #[cfg(test)]
    pub(crate) fn read_payload(
        &self,
        table: &str,
        pk: &str,
    ) -> nostos_core::Result<Option<Vec<u8>>> {
        self.inner.storage().read_payload(table, pk)
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

/// Inject the key-value store the transport persists sync checkpoints to
/// (plan task 6.1 / ADR-0037 §6 Wave 3 — EXPERIMENTAL Web Push enablement).
///
/// `store` is any JS object with the Web Storage shape — `getItem(key)`
/// returning a string or null, and `setItem(key, value)`. Pass `localStorage`
/// itself (the default when unset), a Map-backed shim (the Service-Worker
/// context has no `window`), or a test spy. Passing `null`/`undefined`
/// restores the default. Call BEFORE `NostosSocket.connect` — the active store
/// is captured per-socket at connect time. Default behavior for embedders
/// that never call this is unchanged (`window.localStorage`, a no-op where no
/// window exists).
#[wasm_bindgen(js_name = setKvStore)]
pub fn set_kv_store(store: Option<js_sys::Object>) {
    transport::set_kv_override(
        store
            .map(transport::JsKvStore)
            .map(|s| Rc::new(s) as Rc<dyn transport::KvStore>),
    );
}

/// A live WebSocket sync session in the browser.
///
/// Construct with [`NostosSocket::connect`], which returns a `Promise` that
/// resolves to the socket once the browser has opened it and the subscribe
/// frame is queued (sent on `open`). The server then streams events; each
/// inbound message is decoded by the pure frame-pump, applied to the socket's
/// engine, ACKed per committed batch, and the resulting checkpoint is
/// persisted under the `cairn:checkpoint:<table>` key so a reload can resume —
/// to `localStorage` by default, or to whatever store was injected via
/// [`set_kv_store`] (plan 6.1: the SW-compatible KV seam).
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
        db_handle: Option<js_sys::Object>,
    ) -> Result<NostosSocket, JsValue> {
        transport::connect(url, token, table, where_sql, db_handle).await
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
    /// Send a client write. WS1 contract: this NEVER throws because the socket
    /// is closed — a write while disconnected is captured into the `Outbox`
    /// (`enqueue`) and rendered locally right away (`apply_local`), so the row
    /// is visible INSTANTLY and the write ships on the next (re)connect via the
    /// `onopen` flush loop. Returns the outbox id (Wave 4a: mirrors native
    /// `write` returning the id — the caller can use it to correlate with
    /// `watchWriteStatus` outcomes).
    ///
    /// The call is still `Err` for a *caller bug* — a malformed / non-object
    /// `payload_json`, or an `op` outside `"upsert" | "delete" | "patch"`.
    #[wasm_bindgen(js_name = write)]
    #[allow(clippy::needless_pass_by_value)] // wasm-bindgen JS boundary: owned Option<String>
    pub fn write(
        &self,
        table: &str,
        op: &str,
        pk: &str,
        payload_json: Option<String>,
        client_write_id: &str,
    ) -> Result<f64, JsValue> {
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
        Ok(id as f64)
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

    // ========================================================================
    // Wave 4a: socket-level typed-verb surface.
    // ========================================================================

    /// Materialize the WS2 read-views over `cairn_data` on the socket's engine.
    /// Delegates to [`NostosEngine::apply_schema`]. SqliteWasm only.
    #[wasm_bindgen(js_name = applySchema)]
    pub fn apply_schema(&self, tables: Vec<JsValue>) -> Result<(), JsValue> {
        self.inner.engine.borrow_mut().apply_schema(tables)
    }

    /// Run an arbitrary SELECT, returning a JSON-array string. Delegates to
    /// [`NostosEngine::query`]. SqliteWasm only (Memory returns `"[]"`).
    #[wasm_bindgen(js_name = query)]
    pub fn query(&self, sql: &str) -> Result<String, JsValue> {
        self.inner.engine.borrow().query(sql)
    }

    /// Configure which tables are OR-set / counter CRDTs on the socket's engine.
    /// Delegates to [`NostosEngine::set_crdt_tables`].
    #[wasm_bindgen(js_name = setCrdtTables)]
    pub fn set_crdt_tables(&self, or_set: Vec<String>, counter: Vec<String>) {
        self.inner
            .engine
            .borrow_mut()
            .set_crdt_tables(or_set, counter);
    }

    /// The current pending (non-dead-lettered) write count. For
    /// `watchWriteStatus`.
    #[wasm_bindgen(getter, js_name = pendingCount)]
    pub fn pending_count(&self) -> u32 {
        self.inner.engine.borrow().pending_count()
    }

    /// The current dead-lettered write count. For `watchWriteStatus`.
    #[wasm_bindgen(getter, js_name = deadLetteredCount)]
    pub fn dead_lettered_count(&self) -> u32 {
        self.inner.engine.borrow().dead_lettered_count()
    }

    /// The last error from the most recent dead-lettered write (or null).
    #[wasm_bindgen(getter, js_name = lastError)]
    pub fn last_error(&self) -> Option<String> {
        self.inner.engine.borrow().last_error()
    }

    /// Send an additional subscribe frame for a DIFFERENT table over the
    /// existing socket (Wave 4a multi-table subscribe). The server streams
    /// events for all subscribed tables over the same socket. Call AFTER
    /// `connect` resolves.
    ///
    /// ponytail: the current transport is single-table at the engine level
    /// (the frame-pump acks/persists per-table). A true multi-table port needs
    /// per-table checkpoint tracking in the engine — the server sends events
    /// tagged by `table`, and each table has its own resume_lsn. For now, the
    /// subscribe frame is sent but the checkpoint persists for the FIRST table
    /// only. rewire when convenient.
    #[wasm_bindgen(js_name = subscribe)]
    pub fn subscribe(&self, table: &str, where_sql: Option<String>) -> Result<(), JsValue> {
        let ws = &self.inner.ws;
        if ws.ready_state() != 1 {
            return Err(JsValue::from_str("subscribe: socket not open"));
        }
        let where_clean = where_sql.filter(|s| !s.is_empty());
        let resume = self.inner.engine.borrow().checkpoint();
        let frame = transport::build_subscribe_frame(
            table,
            where_clean.as_deref(),
            if resume > 0.0 {
                Some(resume as u64)
            } else {
                None
            },
        );
        ws.send_with_str(&frame)
            .map_err(|e| JsValue::from_str(&format!("subscribe: send failed: {e:?}")))
    }

    /// Reconnect retaining engine state (Wave 4a). If the socket is closed,
    /// opens a new WebSocket with the stored connection params. The engine
    /// (rows, checkpoint, outbox) survives — the server resumes streaming from
    /// the persisted checkpoint. Returns `true` if a reconnect was initiated,
    /// `false` if the socket was already open.
    ///
    /// ponytail: this creates a new `NostosSocket` internally because the
    /// existing `ws` field is not `RefCell` (changing it would ripple through
    /// the transport). The JS caller should use the returned socket and drop
    /// the old one. A future refactor should make `ws` interior-mutable so
    /// resume can hot-swap in place.
    #[wasm_bindgen(js_name = resume)]
    #[allow(clippy::unused_async)] // async so JS callers can `await`; no Rust await needed (synchronous socket check)
    pub async fn resume(&self) -> Result<bool, JsValue> {
        if self.inner.ws.ready_state() == 1 {
            // Already open — re-send the subscribe frame as a heartbeat.
            let cp = self.inner.engine.borrow().checkpoint();
            let frame = transport::build_subscribe_frame(
                &self.inner.table,
                self.inner.where_sql.as_deref(),
                if cp > 0.0 { Some(cp as u64) } else { None },
            );
            let _ = self.inner.ws.send_with_str(&frame);
            return Ok(false);
        }
        // Socket not open — signal the caller to reconnect via connect().
        // The engine state (rows, checkpoint, outbox) is in the Rc<RefCell<...>>,
        // which the caller can extract before calling connect with the same
        // db_handle. ponytail: a full in-place reconnect requires making `ws`
        // interior-mutable; deferred to avoid transport churn in Wave 4a.
        Err(JsValue::from_str(
            "resume: socket is closed — call NostosSocket.connect() with the \
             same URL + dbHandle to reconnect; the engine state is preserved \
             in the dbHandle's SQLite/OPFS store",
        ))
    }

    // ========================================================================
    // Wave 4c (ADR-0036): CRDT + atomic-batch delegates.
    // ========================================================================
    //
    // These close the Flutter-web typed-surface gap: the CRDT verbs + the
    // transactional `writeBatch` lived ONLY on the in-process `NostosEngine`
    // (4a), but the Flutter-web Worker drives `NostosSocket` (the transport
    // wrapper). The engine is reachable from the socket (`SocketInner.engine`
    // is the same `ApplyEngine<WebStorage>`), so these are THIN DELEGATES that
    // reuse `NostosEngine`'s logic verbatim (HLC mint, `nostos-domain` CRDT
    // algebra, `enqueue_batch` atomicity) — no CRDT algebra is re-implemented
    // here, and `NostosEngine`/native are untouched. The only addition over a
    // bare delegate is the ship step (`ship_if_open`): the engine path
    // enqueues + `apply_local`s but never sends over the wire, so a connected
    // client's CRDT op would otherwise sit in the outbox until the next
    // reconnect flush. Mirrors the ship + reactive-tick half of [`Self::write`].

    /// Add `element` to the add-wins OR-set in row `pk` of `table`. Delegates to
    /// [`NostosEngine::or_set_add`] (mints the client HLC, builds the
    /// `OrSetPayload`, enqueues, `apply_local`s) then ships the write frame now
    /// if the socket is OPEN. Mirrors [`Self::write`]'s enqueue→apply→ship→tick
    /// flow. Returns the outbox id (ADR-0032 T4 / ADR-0030).
    #[wasm_bindgen(js_name = orSetAdd)]
    pub fn or_set_add(&self, table: &str, pk: &str, element: &str) -> Result<f64, JsValue> {
        let id = self
            .inner
            .engine
            .borrow_mut()
            .or_set_add(table, pk, element)?;
        self.ship_if_open(id as u64);
        transport::emit_change(&self.inner.on_change);
        Ok(id)
    }

    /// Remove `element` from the OR-set (a tombstone at a fresh HLC). Add-wins:
    /// a concurrent or later re-add re-activates the element. Delegates to
    /// [`NostosEngine::or_set_remove`].
    #[wasm_bindgen(js_name = orSetRemove)]
    pub fn or_set_remove(&self, table: &str, pk: &str, element: &str) -> Result<f64, JsValue> {
        let id = self
            .inner
            .engine
            .borrow_mut()
            .or_set_remove(table, pk, element)?;
        self.ship_if_open(id as u64);
        transport::emit_change(&self.inner.on_change);
        Ok(id)
    }

    /// Increment the PN-Counter in row `pk` of `table` by `delta` (read-modify-
    /// write). Delegates to [`NostosEngine::counter_increment`] (reads the current
    /// payload, applies the delta to this replica's entry via `nostos-domain`'s
    /// `counter_apply_delta`, enqueues, `apply_local`s) then ships if OPEN.
    #[wasm_bindgen(js_name = counterIncrement)]
    pub fn counter_increment(&self, table: &str, pk: &str, delta: f64) -> Result<f64, JsValue> {
        let id = self
            .inner
            .engine
            .borrow_mut()
            .counter_increment(table, pk, delta)?;
        self.ship_if_open(id as u64);
        transport::emit_change(&self.inner.on_change);
        Ok(id)
    }

    /// Decrement the PN-Counter by `delta` (bumps the negative counter `n`).
    /// Delegates to [`NostosEngine::counter_decrement`].
    #[wasm_bindgen(js_name = counterDecrement)]
    pub fn counter_decrement(&self, table: &str, pk: &str, delta: f64) -> Result<f64, JsValue> {
        let id = self
            .inner
            .engine
            .borrow_mut()
            .counter_decrement(table, pk, delta)?;
        self.ship_if_open(id as u64);
        transport::emit_change(&self.inner.on_change);
        Ok(id)
    }

    /// Enqueue a batch of writes atomically (ADR-0032 T3). All ops commit in
    /// one SQLite txn (SqliteWasm) or one BTreeMap extend (Memory) via the
    /// engine's `enqueue_batch` — a mid-batch failure rolls back the entire
    /// batch. Delegates to [`NostosEngine::write_batch`], then ships each write
    /// now if OPEN. Returns the outbox ids in order.
    #[wasm_bindgen(js_name = writeBatch)]
    #[allow(clippy::needless_pass_by_value)] // wasm_bindgen requires owned Vec<JsValue>
    pub fn write_batch(&self, ops: Vec<JsValue>) -> Result<Vec<f64>, JsValue> {
        let ids = self.inner.engine.borrow_mut().write_batch(ops)?;
        // Ship each now if OPEN. Atomicity is in `enqueue_batch` (one storage
        // txn); the per-write ship is the network send, not the storage
        // boundary, so shipping individually preserves the atomic enqueue.
        for &id in &ids {
            self.ship_if_open(id as u64);
        }
        if !ids.is_empty() {
            transport::emit_change(&self.inner.on_change);
        }
        Ok(ids)
    }

    /// Ship the just-enqueued write `id` if the socket is OPEN: look up the
    /// pending entry, build the wire frame, send it, `mark_done` on success.
    /// Mirrors the ship half of [`Self::write`] — used by the CRDT delegates
    /// + [`Self::write_batch`], which enqueue via the engine (apply_local + HLC
    /// mint) but need the same "ship now if connected" path so a connected
    /// client's CRDT op ships immediately instead of waiting for the next
    /// `onopen` flush. `client_write_id` is synthesized from the outbox id (the
    /// offline-flush convention in `transport::flush_pending`); the CRDT verbs
    /// carry no caller id.
    fn ship_if_open(&self, id: u64) {
        let ws = &self.inner.ws;
        if ws.ready_state() != 1 {
            return; // closed — leave pending for the onopen flush loop.
        }
        // Snapshot the matching pending entry (owned) so the RefCell borrow is
        // released before the synchronous `send_with_str` + `mark_done`.
        let entry = {
            let Ok(pending) = self.inner.engine.borrow_mut().storage_mut().pending() else {
                return;
            };
            pending.into_iter().find(|(pid, _)| *pid == id)
        };
        let Some((_, write)) = entry else {
            return; // already shipped or dead-lettered — nothing to do.
        };
        // `let-else` (clippy::manual_let_else): a malformed payload is left
        // pending for retry rather than shipped as an invalid frame.
        let Ok(frame) = transport::build_write_frame(
            &write.table,
            write.op.as_wire_str(),
            &write.pk,
            write.payload_json.as_deref(),
            &id.to_string(),
        ) else {
            return;
        };
        if ws.send_with_str(&frame).is_ok() {
            let _ = self.inner.engine.borrow_mut().storage_mut().mark_done(id);
        }
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
    use transport::KvStore;
    use transport::{
        build_ack_frame, build_subscribe_frame, build_write_frame, checkpoint_from, checkpoint_key,
        decode_frames, decode_hex, on_message, parse_checkpoint, pump_committed, read_checkpoint,
        write_checkpoint, PumpResult,
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

    // ---- the 6.1 storage seam: checkpoint persistence through KvStore ----
    //
    // The pure read/write helpers take the store as a parameter, so host
    // tests pin the seam with an in-memory fake. The browser halves
    // (`WindowLocalStorage`, `JsKvStore`) are covered by the Playwright
    // browser test (e2e/webpush.spec.cjs) — same testability split as the WS
    // glue (see the module docs).

    /// Host-test fake: a `RefCell<HashMap>` behind the [`KvStore`] trait —
    /// the Rust twin of the Map-backed shim a Service Worker injects.
    struct MemKv(std::cell::RefCell<std::collections::HashMap<String, String>>);

    impl MemKv {
        fn new() -> Self {
            Self(std::cell::RefCell::new(std::collections::HashMap::new()))
        }
    }

    impl transport::KvStore for MemKv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.borrow().get(key).cloned()
        }
        fn set(&self, key: &str, value: &str) {
            self.0
                .borrow_mut()
                .insert(key.to_string(), value.to_string());
        }
    }

    #[test]
    fn checkpoint_round_trips_through_injected_store() {
        // write → the injected store holds the key; read → the same LSN back.
        let kv = MemKv::new();
        write_checkpoint("tasks", 42, &kv);
        assert_eq!(
            kv.0.borrow()
                .get("cairn:checkpoint:tasks")
                .map(String::as_str),
            Some("42"),
            "the injected store received the checkpoint under the pinned key"
        );
        assert_eq!(read_checkpoint("tasks", &kv), Some(42));
    }

    #[test]
    fn injected_store_missing_or_malformed_reads_resume_from_none() {
        // A fresh store (no key) and a corrupt value both read None — the
        // connect path resumes from 0, never panics.
        let kv = MemKv::new();
        assert_eq!(read_checkpoint("tasks", &kv), None, "no key → None");
        kv.set("cairn:checkpoint:tasks", "not a number");
        assert_eq!(read_checkpoint("tasks", &kv), None, "malformed → None");
    }

    #[test]
    fn injected_store_overwrites_are_newest_wins() {
        // Re-connecting and committing a higher LSN replaces the prior value
        // (the engine's high-water is monotonic; the store must not lag it).
        let kv = MemKv::new();
        write_checkpoint("tasks", 10, &kv);
        write_checkpoint("tasks", 20, &kv);
        assert_eq!(read_checkpoint("tasks", &kv), Some(20));
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

#[cfg(test)]
mod typed_verb_tests {
    //! Host unit tests for the Wave 4a typed-verb surface.
    //!
    //! These exercise the typed verbs on the `Memory` backend (which already
    //! has CRDT support — or_set_tables, counter_tables, enqueue_batch,
    //! read_payload). The `SqliteWasmStorage` overrides need a browser +
    //! OPFS — those are covered by the Playwright browser test (ADR-0033).
    //!
    //! What's tested here:
    //! - `setCrdtTables` → storage tags propagate
    //! - `orSetAdd` / `orSetRemove` → HLC mint + element present/absent
    //! - `counterIncrement` / `counterDecrement` → RMW + value accumulates
    //! - `enqueue_batch` through `WebStorage` → atomic delegation
    //! - `read_payload` through `WebStorage` → delegation works
    use super::*;

    // ---- setCrdtTables: CRDT table tags propagate to storage ----

    #[test]
    fn set_crdt_tables_enables_counter_merge() {
        // Before tagging: an upsert clobbers (no merge).
        let mut eng = NostosEngine::new();
        let s = eng.inner.storage_mut();
        let _ = s.enqueue(PendingWrite {
            table: "counters".into(),
            op: WriteOp::Upsert,
            pk: "c1".into(),
            payload_json: Some(r#"{"entries":[{"r":"a","p":5,"n":0}]}"#.into()),
        });
        let _ = s.apply_local(&PendingWrite {
            table: "counters".into(),
            op: WriteOp::Upsert,
            pk: "c1".into(),
            payload_json: Some(r#"{"entries":[{"r":"b","p":3,"n":0}]}"#.into()),
        });
        // Clobbered: only the second write's value survives.
        let payload = eng.inner.storage().read_payload("counters", "c1").unwrap();
        let value = nostos_domain::counter_value(&payload.unwrap()).unwrap();
        assert_eq!(value, 3, "untagged: last-writer-wins (clobber)");

        // After tagging as counter: merge per-replica max.
        eng.set_crdt_tables(vec![], vec!["counters".into()]);
        let s = eng.inner.storage_mut();
        let _ = s.apply_local(&PendingWrite {
            table: "counters".into(),
            op: WriteOp::Upsert,
            pk: "c1".into(),
            payload_json: Some(r#"{"entries":[{"r":"a","p":10,"n":0}]}"#.into()),
        });
        let payload = eng.inner.storage().read_payload("counters", "c1").unwrap();
        let value = nostos_domain::counter_value(&payload.unwrap()).unwrap();
        // Per-replica max merge: existing replica "b" (p=3) + incoming replica
        // "a" (p=10) → total = 13 (both replicas survive, not clobbered).
        assert_eq!(value, 13, "tagged: merged per-replica max (3 + 10 = 13)");
    }

    // ---- orSetAdd: element renders locally ----

    #[test]
    fn or_set_add_renders_element_locally() {
        let mut eng = NostosEngine::new();
        eng.set_crdt_tables(vec!["tags".into()], vec![]);
        let id = eng.or_set_add("tags", "row1", "alice").unwrap();
        assert!(id > 0.0, "orSetAdd returned a positive outbox id");

        // The element should be present in the row's payload.
        let payload = eng.read_payload("tags", "row1").unwrap().unwrap();
        let present = nostos_domain::present_elements(&payload).unwrap();
        assert!(
            present.contains(&"alice".to_string()),
            "element 'alice' is present after orSetAdd"
        );
    }

    #[test]
    fn or_set_remove_tombstones_element() {
        let mut eng = NostosEngine::new();
        eng.set_crdt_tables(vec!["tags".into()], vec![]);
        eng.or_set_add("tags", "row1", "bob").unwrap();
        // Verify present.
        let payload = eng.read_payload("tags", "row1").unwrap().unwrap();
        assert!(
            nostos_domain::present_elements(&payload)
                .unwrap()
                .contains(&"bob".to_string()),
            "element 'bob' present after add"
        );
        // Remove: tombstone should make it absent.
        eng.or_set_remove("tags", "row1", "bob").unwrap();
        let payload = eng.read_payload("tags", "row1").unwrap().unwrap();
        let present = nostos_domain::present_elements(&payload).unwrap();
        assert!(
            !present.contains(&"bob".to_string()),
            "element 'bob' absent after remove"
        );
    }

    // ---- counterIncrement / counterDecrement: RMW accumulates ----

    #[test]
    fn counter_increment_accumulates_same_replica() {
        let mut eng = NostosEngine::new();
        eng.set_crdt_tables(vec![], vec!["likes".into()]);

        // First increment: starts from 0 (no existing payload).
        eng.counter_increment("likes", "post1", 5.0).unwrap();
        let payload = eng.read_payload("likes", "post1").unwrap().unwrap();
        let v1 = nostos_domain::counter_value(&payload).unwrap();
        assert_eq!(v1, 5, "first increment: value = 5");

        // Second increment: RMW reads existing (5), applies delta (+3) = 8.
        eng.counter_increment("likes", "post1", 3.0).unwrap();
        let payload = eng.read_payload("likes", "post1").unwrap().unwrap();
        let v2 = nostos_domain::counter_value(&payload).unwrap();
        assert_eq!(v2, 8, "second increment: RMW accumulates (5 + 3 = 8)");
    }

    #[test]
    fn counter_decrement_subtracts_value() {
        let mut eng = NostosEngine::new();
        eng.set_crdt_tables(vec![], vec!["score".into()]);
        eng.counter_increment("score", "g1", 10.0).unwrap();
        eng.counter_decrement("score", "g1", 4.0).unwrap();
        let payload = eng.read_payload("score", "g1").unwrap().unwrap();
        let v = nostos_domain::counter_value(&payload).unwrap();
        assert_eq!(v, 6, "increment 10 - decrement 4 = 6");
    }

    #[test]
    fn counter_increment_multiple_replicas_merge() {
        // Two engines with different replica ids each increment the same row;
        // the per-replica max merge converges (the total is the sum of each
        // replica's positive counter).
        let mut eng1 = NostosEngine::new();
        eng1.set_crdt_tables(vec![], vec!["views".into()]);
        eng1.counter_increment("views", "page", 7.0).unwrap();

        // Simulate a second replica: read eng1's payload, merge with eng2's.
        let payload1 = eng1.read_payload("views", "page").unwrap().unwrap();

        let mut eng2 = NostosEngine::new();
        eng2.set_crdt_tables(vec![], vec!["views".into()]);
        eng2.counter_increment("views", "page", 3.0).unwrap();

        // Manually merge (simulating what the server does):
        let payload2 = eng2.read_payload("views", "page").unwrap().unwrap();
        let merged = nostos_domain::merge_counter_or_lww(&payload1, &payload2);
        let total = nostos_domain::counter_value(&merged).unwrap();
        assert_eq!(total, 10, "merged counter = 7 + 3 = 10");
    }

    // ---- enqueue_batch: atomic delegation through WebStorage ----

    #[test]
    fn enqueue_batch_returns_ids_in_order() {
        // InMemoryStorage's enqueue_batch is atomic (BTreeMap extend) and
        // returns ids in order. This test proves the WebStorage delegation
        // routes correctly.
        let mut eng = NostosEngine::new();
        let writes = vec![
            PendingWrite {
                table: "t".into(),
                op: WriteOp::Upsert,
                pk: "1".into(),
                payload_json: Some(r#"{"v":1}"#.into()),
            },
            PendingWrite {
                table: "t".into(),
                op: WriteOp::Upsert,
                pk: "2".into(),
                payload_json: Some(r#"{"v":2}"#.into()),
            },
            PendingWrite {
                table: "t".into(),
                op: WriteOp::Upsert,
                pk: "3".into(),
                payload_json: Some(r#"{"v":3}"#.into()),
            },
        ];
        let ids = eng.inner.storage_mut().enqueue_batch(writes).unwrap();
        assert_eq!(ids.len(), 3, "batch returned 3 ids");
        // Ids are sequential and ascending.
        assert!(ids[0] < ids[1] && ids[1] < ids[2], "ids are ascending");
        // All three are pending.
        let pending = eng.inner.storage().pending().unwrap();
        assert_eq!(pending.len(), 3, "all 3 writes pending");
    }

    // ---- read_payload: delegation through WebStorage ----

    #[test]
    fn read_payload_returns_none_for_absent_row() {
        let eng = NostosEngine::new();
        let result = eng.read_payload("absent", "x").unwrap();
        assert!(result.is_none(), "absent row → None");
    }

    #[test]
    fn read_payload_returns_bytes_after_apply() {
        let mut eng = NostosEngine::new();
        let s = eng.inner.storage_mut();
        let _ = s.enqueue(PendingWrite {
            table: "t".into(),
            op: WriteOp::Upsert,
            pk: "k".into(),
            payload_json: Some(r#"{"hello":"world"}"#.into()),
        });
        let _ = s.apply_local(&PendingWrite {
            table: "t".into(),
            op: WriteOp::Upsert,
            pk: "k".into(),
            payload_json: Some(r#"{"hello":"world"}"#.into()),
        });
        let bytes = eng.read_payload("t", "k").unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["hello"], "world");
    }

    // ---- query on Memory returns empty (no SQL engine) ----

    #[test]
    fn query_on_memory_returns_empty_array() {
        let eng = NostosEngine::new();
        let result = eng.query("SELECT * FROM tasks").unwrap();
        assert_eq!(result, "[]", "Memory backend has no SQL → empty array");
    }

    // ---- write status getters ----

    #[test]
    fn pending_count_reflects_enqueued_writes() {
        let mut eng = NostosEngine::new();
        assert_eq!(eng.pending_count(), 0, "fresh engine: 0 pending");
        let s = eng.inner.storage_mut();
        let _ = s.enqueue(PendingWrite {
            table: "t".into(),
            op: WriteOp::Upsert,
            pk: "1".into(),
            payload_json: Some(r"{}".into()),
        });
        let _ = s.enqueue(PendingWrite {
            table: "t".into(),
            op: WriteOp::Upsert,
            pk: "2".into(),
            payload_json: Some(r"{}".into()),
        });
        assert_eq!(eng.pending_count(), 2, "2 writes enqueued");
    }

    #[test]
    fn replica_id_is_unique_per_engine() {
        let eng1 = NostosEngine::new();
        let eng2 = NostosEngine::new();
        assert_ne!(
            eng1.replica_id, eng2.replica_id,
            "two engines have distinct replica ids"
        );
    }

    #[test]
    fn hlc_state_advances_monotonically() {
        let eng = NostosEngine::new();
        let h1 = eng.mint_hlc();
        let h2 = eng.mint_hlc();
        assert!(h2 > h1, "second mint > first (monotonic HLC)");
    }
}
