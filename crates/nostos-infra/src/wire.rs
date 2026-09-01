//! Wire codec — translate `ReplicationEvent` into a JSON frame on the wire.
//!
//! The Week-1 wire format is JSON (human-debuggable; the benchmark doesn't
//! depend on encoding speed yet). Phase 2 will add a compact binary mode
//! (length-prefixed, protobuf-ish) selected by a header byte — but JSON first
//! keeps the demo legible and the comparison to PowerSync fair (their protocol
//! is also JSON-shaped on the wire).
//!
//! Frames are also what the benchmark client counts — one received frame ==
//! one delivered event.

use nostos_domain::{Operation, ReplicationEvent, RowOp};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _; // ponytail: single write!() in push_json_string

/// One frame on the wire (server → client). A client receives a stream of
/// these. Reuses [`Operation`] directly (it already serializes to
/// `insert`/`update`/`delete`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireFrame {
    pub lsn: u64,
    pub op: Operation,
    pub table: String,
    pub pk: String,
    /// Hex-encoded payload (omitted for deletes to keep frames small).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub txn_id: Option<u64>,
}

/// A message from the client (client → server). Tagged by `type` so the same
/// socket carries the initial subscription, subsequent ACKs, write requests,
/// and (future) predicate updates. This is the inbound decode path that didn't
/// exist before T0-4 — `wire.rs` was encode-only.
///
/// - `subscribe` — the first frame: what to receive + where to resume from.
/// - `ack` — the client confirms it has applied through `lsn`; drives the
///   ack-driven slot advance (ADR-0009).
/// - `write` — a client-initiated upsert/delete applied to the source DB via
///   the `WriteBack` port (ADR-0013). Only valid AFTER the subscribe handshake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMessage {
    /// Subscribe to a table with optional filters, optionally resuming.
    Subscribe {
        table: String,
        #[serde(default)]
        filters: Vec<FilterClause>,
        /// Optional safe-SQL-subset expression (ADR-0012 compiler). Compiled
        /// server-side; ANDed with `filters` and with server-enforced clauses
        /// (e.g. tenant scoping, ADR-0011) so a client can never widen scope
        /// past its own tenant. Invalid SQL closes the socket with a reason of
        /// `"invalid where_sql: <ParseError>"` before any event flows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_sql: Option<String>,
        /// Resume from this LSN — the client has already applied through here,
        /// so the server seeds its ack cursor and skips re-delivering ≤ it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_lsn: Option<u64>,
        /// The client's last-seen server slot epoch (ADR-0025 slice 4b). The
        /// server compares it to its current `slot_epoch`: a mismatch means the
        /// slot was dropped + recreated (the WAL lineage broke) so the client
        /// cannot backfill from the dead op-stream → full snapshot. A match,
        /// with a `resume_lsn` inside the op-log window, → op-log replay
        /// instead. `None` (an old client that doesn't track epochs) is treated
        /// as a mismatch → snapshot (the safe default; replay is opt-in).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<u64>,
        /// The rules checksum the client last synced under (ADR-0031, D2).
        /// `u64`, symmetric with `epoch` directly above it — no SDK parses this
        /// frame by hand (see the Task 12 survey), so there is no JS-precision
        /// argument for a string encoding, and an integer keeps
        /// `skip_serializing_if` behaviour identical to `epoch`.
        /// `None` = a pre-D2 client → composed-epoch fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules_checksum: Option<u64>,
    },
    /// Acknowledge applied progress (highest applied LSN).
    Ack { lsn: u64 },
    /// Apply a client-submitted write to the source database (ADR-0013). The
    /// server acks with a `WriteResult` frame carrying the same
    /// `client_write_id` so the client can correlate the response. The written
    /// row then flows back out through normal replication to every subscriber
    /// (including the writer, where the idempotent apply is a no-op).
    Write {
        /// Target table — MUST be in the server's `NOSTOS_WRITE_TABLES` allowlist.
        table: String,
        /// `"upsert"`, `"delete"`, or `"patch"` (P3 PowerSync PATCH parity).
        op: String,
        /// Primary-key value (v1 convention: pk column is `id`).
        pk: String,
        /// The row image for an upsert: a JSON object of column → value, the
        /// same tuple-image shape the read path delivers. For a patch, the
        /// PARTIAL column set to update (columns absent are untouched). Absent
        /// (`None`) for deletes. A non-object (array / scalar / null) is
        /// rejected as `InvalidPayload` before any SQL is built.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
        /// Client-supplied correlation id, echoed back in the `WriteResult`.
        /// Lets the client match each ack to its request (the round-trip is
        /// asynchronous: the write task may be behind the client's send loop).
        client_write_id: String,
    },
    /// Subscribe to a named, client-parameterized sync stream (P5 sync
    /// streams — `docs/plans/p5-sync-streams-design.md` §1). `id` is
    /// client-chosen (unique per socket; reuse = idempotent replace);
    /// `stream` names a server-defined stream; `params` binds the
    /// stream's `:param` placeholders — value-level binding, never
    /// textual, so no client byte ever enters SQL (design Decision 2).
    /// Streams ride the socket's ONE global checkpoint (ADR-0009), so there
    /// is deliberately no per-stream `resume_lsn`/`epoch`.
    #[serde(rename = "subscribe_stream")]
    SubscribeStream {
        id: String,
        stream: String,
        /// Bindings for the stream definition's `:param` placeholders.
        /// Typed JSON values; the server turns each into a `ColumnValue`
        /// leaf at subscribe time. Absent = no params.
        #[serde(default)]
        params: serde_json::Map<String, serde_json::Value>,
    },
    /// Drop a previously-subscribed sync stream by its client-chosen `id`
    /// (P5 §1). v1 leaves local rows in place — eviction is separate;
    /// PowerSync behaves the same.
    #[serde(rename = "unsubscribe_stream")]
    UnsubscribeStream { id: String },
}

/// One column-equality filter in a `Subscribe` message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilterClause {
    pub column: String,
    pub value: String,
}

/// Decode a client message from a text/binary frame. Returns `None` on a
/// malformed frame (the transport closes the socket in that case).
///
/// This is the inbound counterpart to [`encode_event`] — both JSON, so the wire
/// stays human-debuggable (the benchmark protocol is JSON-shaped; PowerSync's
/// is too).
#[must_use]
pub fn decode_client_message(data: &[u8]) -> Option<ClientMessage> {
    serde_json::from_slice::<ClientMessage>(data).ok()
}

/// Encode an event to a JSON frame (as bytes). Used by the server transport.
///
/// Stateless and allocation-free of shared context, so it's a free function —
/// callable concurrently from many tasks with nothing to Arc.
#[must_use]
pub fn encode_event(event: &ReplicationEvent) -> Vec<u8> {
    let (op, table, pk, payload) = match &event.op {
        RowOp::Insert { table, pk, payload } => (
            Operation::Insert,
            table.clone(),
            pk.clone(),
            Some(hex::encode(payload)),
        ),
        RowOp::Update { table, pk, payload } => (
            Operation::Update,
            table.clone(),
            pk.clone(),
            Some(hex::encode(payload)),
        ),
        RowOp::Delete { table, pk, .. } => (Operation::Delete, table.clone(), pk.clone(), None),
    };
    let frame = WireFrame {
        lsn: event.lsn.raw(),
        op,
        table,
        pk,
        payload,
        txn_id: event.txn_id,
    };
    // Safe: our types are all JSON-serializable.
    serde_json::to_vec(&frame).expect("wire frame must serialize")
}

/// Encode a `WriteResult` frame (server → client) as JSON bytes. This is the
/// ack for a client's `Write` message (ADR-0013). Beside [`encode_event`] —
/// it's a distinct server→client shape (no `lsn`/`op`/`table`; it carries the
/// correlation id + outcome), so it gets its own `"type":"write_result"` tag
/// rather than reusing [`WireFrame`].
///
/// `error` is `Some(msg)` when `ok` is `false`; `None` (omitted on the wire)
/// when `ok` is `true`. The `client_write_id` echoes the request so the client
/// can correlate the response.
///
/// `retryable` closes ADR-0013 v2's transient-vs-permanent gap: a rejection
/// the server KNOWS no retry can fix (allowlist, payload shape) is flagged
/// `"retryable":false` so the client dead-letters on the first rejection
/// instead of burning `dead_letter_max_attempts` wire cycles per write. The
/// flag is emitted ONLY when false — absent means retryable, so the ok path
/// and pre-flag servers keep the exact old shape.
#[must_use]
pub fn encode_write_result(
    client_write_id: &str,
    ok: bool,
    error: Option<&str>,
    retryable: bool,
) -> Vec<u8> {
    // Hand-built JSON keeps this allocation-light and avoids inventing a struct
    // for one outbound shape. The fields are simple (string/bool), so escaping
    // the two free-form strings (id + error) is the only care needed.
    let mut out = String::with_capacity(64 + client_write_id.len());
    out.push_str("{\"type\":\"write_result\",\"client_write_id\":");
    push_json_string(&mut out, client_write_id);
    out.push_str(",\"ok\":");
    out.push_str(if ok { "true" } else { "false" });
    if !retryable {
        out.push_str(",\"retryable\":false");
    }
    if let Some(err) = error {
        out.push_str(",\"error\":");
        push_json_string(&mut out, err);
    }
    out.push('}');
    out.into_bytes()
}

/// Encode a `stream_error` frame (server → client) as JSON bytes — the
/// non-fatal reject for a `subscribe_stream`/`unsubscribe_stream` request
/// (unknown stream, bad params; P5 sync streams,
/// `docs/plans/p5-sync-streams-design.md` §1). Mirrors the non-fatal
/// mid-session subscribe reject instead of closing the socket. Same
/// hand-built, allocation-light style as [`encode_write_result`]; both
/// strings are free-form, so both go through JSON escaping.
#[must_use]
pub fn encode_stream_error(id: &str, error: &str) -> Vec<u8> {
    let mut out = String::with_capacity(40 + id.len() + error.len());
    out.push_str("{\"type\":\"stream_error\",\"id\":");
    push_json_string(&mut out, id);
    out.push_str(",\"error\":");
    push_json_string(&mut out, error);
    out.push('}');
    out.into_bytes()
}

/// Minimal JSON string escaping (in-place append) — used by
/// [`encode_write_result`] for the `client_write_id` and `error` fields, which
/// are free-form client/adapter strings.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Encode a slice of events as **one JSON array of frames** (C3 batched-writes).
///
/// When the per-session write task drains more than one pending frame under
/// backlog, it coalesces them into a single WebSocket message carrying a JSON
/// array — one wire write amortizes N frame-encode + socket-send costs. The
/// receiver decodes via [`decode_frames`], which accepts both the array form
/// (this function's output) and the legacy single-object form ([`encode_event`]),
/// so the server can start batching with no wire-version bump and old
/// single-frame messages stay valid.
///
/// A single-event slice produces a one-element array; callers that know they
/// have exactly one event should prefer [`encode_event`] (no array wrapper) to
/// keep the low-rate path identical to the pre-batching wire.
#[must_use]
pub fn encode_events(events: &[&ReplicationEvent]) -> Vec<u8> {
    // Serialize each frame to a serde_json::Value, then write them into an
    // array in one pass. We go through `Serializer` per frame (rather than
    // building a `Vec<Value>`) to avoid retaining the intermediate tree.
    let mut out = Vec::with_capacity(events.len() * 128 + 4);
    out.push(b'[');
    for (i, ev) in events.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        // Serialize straight into the output buffer.
        let frame = event_to_frame_value(ev);
        serde::Serialize::serialize(&frame, &mut serde_json::Serializer::new(&mut out))
            .expect("wire frame must serialize");
    }
    out.push(b']');
    out
}

/// Build the [`WireFrame`] for an event (the shared shape between
/// [`encode_event`] and [`encode_events`]).
fn event_to_frame_value(event: &ReplicationEvent) -> WireFrame {
    let (op, table, pk, payload) = match &event.op {
        RowOp::Insert { table, pk, payload } => (
            Operation::Insert,
            table.clone(),
            pk.clone(),
            Some(hex::encode(payload)),
        ),
        RowOp::Update { table, pk, payload } => (
            Operation::Update,
            table.clone(),
            pk.clone(),
            Some(hex::encode(payload)),
        ),
        RowOp::Delete { table, pk, .. } => (Operation::Delete, table.clone(), pk.clone(), None),
    };
    WireFrame {
        lsn: event.lsn.raw(),
        op,
        table,
        pk,
        payload,
        txn_id: event.txn_id,
    }
}

/// Decode a WebSocket message into zero or more frames (C3 batched-writes).
///
/// Accepts BOTH wire forms so the server can batch without a version bump:
/// - **single object** `{...}` (legacy, [`encode_event`]) → one frame, and
/// - **JSON array** `[{...},{...}]` ([`encode_events`]) → N frames.
///
/// Returns an empty `Vec` on a malformed message (callers ignore it, matching
/// the prior `Option`-based single-frame decode's "drop malformed" behavior).
///
/// The dispatch is O(1): it peeks the first significant byte (`[` → array,
/// `{` → object) rather than attempting a full parse twice.
#[must_use]
pub fn decode_frames(data: &[u8]) -> Vec<WireFrame> {
    // Peek the first significant (non-whitespace) byte to pick the branch.
    let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
    match first {
        Some(b'[') => serde_json::from_slice::<Vec<WireFrame>>(data).unwrap_or_default(),
        Some(b'{') => match serde_json::from_slice::<WireFrame>(data) {
            Ok(f) => vec![f],
            Err(_) => Vec::new(),
        },
        // Empty message or anything else → no frames.
        _ => Vec::new(),
    }
}

/// Tiny hex encoder — avoids pulling a `hex` crate for one fn.
mod hex {
    use std::fmt::Write;

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

// ---- Snapshot-reconcile control frames (ADR-0014 offline-delete fix) ----
//
// The wire already carries one non-`WireFrame` control shape —
// `{"type":"write_result",...}` — for client-write acks. The snapshot
// boundary uses the same pattern: a tagged JSON object that does NOT decode
// as a `WireFrame` (no `lsn`/`op`/`table`+`pk` pair), so the client pump
// intercepts it BEFORE `decode_frames` and drives the reconcile engine.
// `snapshot_begin{T}` is delivered immediately before a snapshot's rows;
// `snapshot_end{T}` immediately after. Old clients that don't check just see
// `decode_frames` return an empty Vec for these objects — no crash
// (back-compat: the wire is additive JSON with a `type` tag).

/// Encode a snapshot boundary control frame as JSON bytes.
///
/// `begin = true` → `{"type":"snapshot_begin","table":"<t>"}`;
/// `begin = false` → `{"type":"snapshot_end","table":"<t>"}`.
/// Beside [`encode_event`] / [`encode_write_result`] — a distinct control
/// shape, never batched with replication events (the writer task drains it
/// through the same `server_frames_tx` channel as write-acks).
#[must_use]
pub fn encode_snapshot_boundary(table: &str, begin: bool) -> Vec<u8> {
    encode_snapshot_boundary_impl(table, None, begin)
}

/// Encode a snapshot boundary control frame for a per-stream TARGETED
/// snapshot (P5 sync streams — `docs/plans/p5-sync-streams-design.md`
/// §1/§3): the same shape as [`encode_snapshot_boundary`] plus a
/// `"stream"` key carrying the client-chosen stream id, so the client can
/// attribute the bracketed rows to the right stream. Table-level snapshots
/// keep using [`encode_snapshot_boundary`] — no `stream` key on the wire.
#[must_use]
pub fn encode_snapshot_boundary_for_stream(table: &str, stream: &str, begin: bool) -> Vec<u8> {
    encode_snapshot_boundary_impl(table, Some(stream), begin)
}

fn encode_snapshot_boundary_impl(table: &str, stream: Option<&str>, begin: bool) -> Vec<u8> {
    // Hand-built JSON keeps this allocation-light and matches
    // `encode_write_result`'s style. Only the table + stream names are
    // free-form, so they're the only fields that need JSON escaping.
    let mut out = String::with_capacity(48 + table.len() + stream.map_or(0, str::len));
    out.push_str("{\"type\":\"");
    out.push_str(if begin {
        "snapshot_begin"
    } else {
        "snapshot_end"
    });
    out.push_str("\",\"table\":");
    push_json_string(&mut out, table);
    if let Some(stream) = stream {
        out.push_str(",\"stream\":");
        push_json_string(&mut out, stream);
    }
    out.push('}');
    out.into_bytes()
}

/// Decode a snapshot boundary control frame from a raw WS message. Returns
/// `Some((table, begin))` only when the payload is a single JSON object with
/// `"type":"snapshot_begin"` or `"type":"snapshot_end"`; returns `None` for
/// everything else (arrays of frames, single `WireFrame` objects,
/// `write_result` acks, malformed bytes, empty payloads).
///
/// The client pump calls this BEFORE [`decode_frames`] so control frames
/// never enter the row-apply path.
///
/// Kept at its pre-P5 shape for existing callers; [`decode_snapshot_boundary`]
/// is the superset that also exposes the optional P5 `stream` id.
#[must_use]
pub fn decode_control_frame(data: &[u8]) -> Option<(String, bool)> {
    decode_snapshot_boundary(data).map(|(table, _, begin)| (table, begin))
}

/// Decode a snapshot boundary control frame, exposing the optional `stream`
/// id (P5 sync streams — `docs/plans/p5-sync-streams-design.md` §1).
/// Returns `(table, stream, begin)`; `stream` is `None` for table-level
/// snapshots (the `stream` key is simply absent on the wire). Unknown keys
/// are ignored (forward compatibility — an older peer's extra fields never
/// break decode). Same accept/reject discipline as the pre-P5 decoder:
/// `None` for arrays, row frames, `write_result` acks, malformed bytes,
/// and snapshot frames missing `table`.
#[must_use]
pub fn decode_snapshot_boundary(data: &[u8]) -> Option<(String, Option<String>, bool)> {
    // Cheap peek: arrays (`[`) and anything that isn't an object (`{`) cannot
    // be a control frame — bail before paying for a full parse. This mirrors
    // `decode_frames`' first-byte dispatch.
    let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
    if first != Some(b'{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(data).ok()?;
    let ty = v.get("type")?.as_str()?;
    let begin = match ty {
        "snapshot_begin" => true,
        "snapshot_end" => false,
        _ => return None,
    };
    let table = v.get("table")?.as_str()?.to_string();
    let stream = v
        .get("stream")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some((table, stream, begin))
}

/// Encode a `resume_info` control frame advertising the server's current slot
/// epoch (ADR-0025 reconnect-resume gate). Emitted once at subscribe on BOTH the
/// snapshot + replay paths so the client can persist + resend its last-seen
/// epoch on reconnect. Beside [`encode_snapshot_boundary`] / write-acks — its
/// own wire shape, never batched with replication events.
///
/// Advertise the resume epoch, and — for D2 clients — the active rules
/// checksum. `rules_checksum: None` omits the key entirely, so the frame a
/// pre-D2 client sees is byte-identical to today's.
#[must_use]
pub fn encode_resume_info(epoch: u64, rules_checksum: Option<u64>) -> Vec<u8> {
    // u64 → decimal; no free-form fields → no escaping needed.
    let mut out = String::from("{\"type\":\"resume_info\",\"epoch\":");
    out.push_str(&epoch.to_string());
    if let Some(checksum) = rules_checksum {
        out.push_str(",\"rules_checksum\":");
        out.push_str(&checksum.to_string());
    }
    out.push('}');
    out.into_bytes()
}

/// Decode a `resume_info` control frame. Returns the epoch and the checksum
/// when present; `None` for everything else (arrays, row frames, snapshot
/// boundaries, malformed bytes). Unknown keys are ignored (forward
/// compatibility). The client pump calls this BEFORE [`decode_frames`] so it
/// never enters the row-apply path.
#[must_use]
pub fn decode_resume_info(data: &[u8]) -> Option<(u64, Option<u64>)> {
    let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
    if first != Some(b'{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(data).ok()?;
    if v.get("type")?.as_str()? != "resume_info" {
        return None;
    }
    let epoch = v.get("epoch")?.as_u64()?;
    let rules_checksum = v.get("rules_checksum").and_then(serde_json::Value::as_u64);
    Some((epoch, rules_checksum))
}

/// Decode a `stream_error` control frame (server → client; P5 sync streams
/// §1), returning `(id, error)`. `None` for anything else — same accept/reject
/// discipline as [`decode_resume_info`]: unknown/malformed shapes are not
/// stream errors.
#[must_use]
pub fn decode_stream_error(data: &[u8]) -> Option<(String, String)> {
    let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
    if first != Some(b'{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(data).ok()?;
    if v.get("type")?.as_str()? != "stream_error" {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    let error = v.get("error")?.as_str()?.to_string();
    Some((id, error))
}

/// Encode a `resync_required` frame (server -> client; ADR-0040) as JSON
/// bytes - the non-fatal "your stream shed events; reconcile now" signal.
/// Emitted once per shed episode when `NOSTOS_RESYNC_SIGNAL` is enabled.
/// Same hand-built style as [`encode_stream_error`].
#[must_use]
pub fn encode_resync_required(table: &str, reason: &str) -> Vec<u8> {
    let mut out = String::with_capacity(60 + table.len() + reason.len());
    out.push_str("{\"type\":\"resync_required\",\"table\":");
    push_json_string(&mut out, table);
    out.push_str(",\"reason\":");
    push_json_string(&mut out, reason);
    out.push('}');
    out.into_bytes()
}

/// Decode a `resync_required` control frame (server -> client; ADR-0040),
/// returning `(table, reason)`. `None` for anything else - same accept/reject
/// discipline as [`decode_stream_error`].
#[must_use]
pub fn decode_resync_required(data: &[u8]) -> Option<(String, String)> {
    let first = data.iter().copied().find(|b| !b.is_ascii_whitespace());
    if first != Some(b'{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(data).ok()?;
    if v.get("type")?.as_str()? != "resync_required" {
        return None;
    }
    let table = v.get("table")?.as_str()?.to_string();
    let reason = v.get("reason")?.as_str()?.to_string();
    Some((table, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nostos_domain::Lsn;

    fn ev() -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(42),
            RowOp::Insert {
                table: "tasks".into(),
                pk: "1".into(),
                payload: Bytes::from_static(b"hi"),
            },
        )
        .with_txn(7)
    }

    #[test]
    fn encode_roundtrips_through_json() {
        let bytes = encode_event(&ev());
        // Decode the JSON to confirm the shape (no decode() on the codec —
        // callers use serde_json directly if they need to read frames).
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["lsn"], 42);
        assert_eq!(v["op"], "insert");
        assert_eq!(v["table"], "tasks");
        assert_eq!(v["pk"], "1");
        assert_eq!(v["txn_id"], 7);
        // payload hex of b"hi" == "6869"
        assert_eq!(v["payload"], "6869");
    }

    // ---- C3 batched-writes codec tests ----

    fn ev_n(i: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(i),
            RowOp::Insert {
                table: "tasks".into(),
                pk: i.to_string(),
                payload: Bytes::from_static(b"hi"),
            },
        )
        .with_txn(i)
    }

    #[test]
    fn encode_events_produces_a_json_array() {
        let e1 = ev_n(1);
        let e2 = ev_n(2);
        let e3 = ev_n(3);
        let bytes = encode_events(&[&e1, &e2, &e3]);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v.as_array().expect("batch encodes as a JSON array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["lsn"], 1);
        assert_eq!(arr[1]["lsn"], 2);
        assert_eq!(arr[2]["lsn"], 3);
    }

    #[test]
    fn decode_frames_accepts_an_array() {
        let e1 = ev_n(10);
        let e2 = ev_n(11);
        let bytes = encode_events(&[&e1, &e2]);
        let frames = decode_frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].lsn, 10);
        assert_eq!(frames[1].lsn, 11);
        assert_eq!(frames[0].pk, "10");
    }

    #[test]
    fn decode_frames_accepts_a_single_object_back_compat() {
        // Legacy single-frame messages ([`encode_event`]) must still decode.
        let bytes = encode_event(&ev_n(7));
        let frames = decode_frames(&bytes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].lsn, 7);
    }

    #[test]
    fn decode_frames_empty_array_is_empty() {
        let frames = decode_frames(b"[]");
        assert!(frames.is_empty());
    }

    #[test]
    fn decode_frames_malformed_is_empty() {
        // Garbage → empty Vec (caller ignores), matching the prior Option(None).
        assert!(decode_frames(b"not json").is_empty());
        assert!(decode_frames(b"").is_empty());
        assert!(decode_frames(b"  ").is_empty());
        // Array with a non-WireFrame element → parse fails → empty.
        assert!(decode_frames(b"[\"not a frame\"]").is_empty());
    }

    #[test]
    fn decode_frames_handles_leading_whitespace() {
        // A decoder that dispatches on the first byte must skip whitespace.
        let bytes = encode_event(&ev_n(5));
        let padded = format!("   {}", std::str::from_utf8(&bytes).unwrap());
        let frames = decode_frames(padded.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].lsn, 5);
    }

    #[test]
    fn encode_then_decode_batch_roundtrips() {
        let events: Vec<ReplicationEvent> = (1..=5).map(ev_n).collect();
        let refs: Vec<&ReplicationEvent> = events.iter().collect();
        let bytes = encode_events(&refs);
        let frames = decode_frames(&bytes);
        assert_eq!(frames.len(), 5);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.lsn, (i as u64) + 1);
            assert_eq!(f.pk, ((i as u64) + 1).to_string());
        }
    }

    #[test]
    fn delete_has_no_payload() {
        let del = ReplicationEvent::new(
            Lsn::new(1),
            RowOp::Delete {
                table: "tasks".into(),
                pk: "9".into(),
                old_payload: None,
            },
        );
        let bytes = encode_event(&del);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("payload").is_none() || v["payload"].is_null());
        assert_eq!(v["op"], "delete");
    }

    #[test]
    fn decode_subscribe_with_filters_and_resume() {
        let json = r#"{"type":"subscribe","table":"tasks","filters":[{"column":"org_id","value":"acme"}],"resume_lsn":12345}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid subscribe");
        let ClientMessage::Subscribe {
            table,
            filters,
            where_sql,
            resume_lsn,
            epoch: _,
            rules_checksum: _,
        } = msg
        else {
            panic!("expected Subscribe");
        };
        assert_eq!(table, "tasks");
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].column, "org_id");
        assert_eq!(filters[0].value, "acme");
        assert_eq!(where_sql, None);
        assert_eq!(resume_lsn, Some(12345));
    }

    #[test]
    fn decode_subscribe_minimal() {
        // No filters, no resume — both default.
        let json = r#"{"type":"subscribe","table":"users"}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid subscribe");
        let ClientMessage::Subscribe {
            table,
            filters,
            where_sql,
            resume_lsn,
            epoch: _,
            rules_checksum: _,
        } = msg
        else {
            panic!("expected Subscribe");
        };
        assert_eq!(table, "users");
        assert!(filters.is_empty());
        assert_eq!(where_sql, None);
        assert_eq!(resume_lsn, None);
    }

    // ---- D2: rules_checksum on Subscribe + resume_info (ADR-0031) ----

    #[test]
    fn subscribe_without_rules_checksum_decodes() {
        // Today's exact JSON (pre-D2 client) still parses, with
        // rules_checksum: None — the fallback path.
        let json = r#"{"type":"subscribe","table":"tasks"}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid subscribe");
        let ClientMessage::Subscribe { rules_checksum, .. } = msg else {
            panic!("expected Subscribe");
        };
        assert_eq!(rules_checksum, None);
    }

    #[test]
    fn subscribe_with_rules_checksum_decodes() {
        let json = r#"{"type":"subscribe","table":"tasks","rules_checksum":777}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid subscribe");
        let ClientMessage::Subscribe { rules_checksum, .. } = msg else {
            panic!("expected Subscribe");
        };
        assert_eq!(rules_checksum, Some(777));
    }

    #[test]
    fn resume_info_omits_checksum_when_none() {
        let bytes = encode_resume_info(5, None);
        // Assert on the raw bytes: no `rules_checksum` key at all, so a
        // pre-D2 client sees a frame byte-identical to today's.
        assert!(!String::from_utf8(bytes.clone())
            .unwrap()
            .contains("rules_checksum"));
        assert_eq!(decode_resume_info(&bytes), Some((5, None)));
    }

    #[test]
    fn resume_info_roundtrips_checksum() {
        let bytes = encode_resume_info(5, Some(999));
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["rules_checksum"], 999);
        assert_eq!(decode_resume_info(&bytes), Some((5, Some(999))));
    }

    #[test]
    fn decode_resume_info_ignores_unknown_keys() {
        let bytes = br#"{"type":"resume_info","epoch":5,"foo":1}"#;
        assert_eq!(decode_resume_info(bytes), Some((5, None)));
    }

    #[test]
    fn decode_ack() {
        let json = r#"{"type":"ack","lsn":987}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid ack");
        assert_eq!(msg, ClientMessage::Ack { lsn: 987 });
    }

    #[test]
    fn decode_malformed_returns_none() {
        assert!(decode_client_message(b"not json").is_none());
        assert!(decode_client_message(b"{\"type\":\"unknown\"}").is_none());
        // Missing required `lsn` on an ack.
        assert!(decode_client_message(b"{\"type\":\"ack\"}").is_none());
    }

    // ---- D2 write-back wire tests ----

    #[test]
    fn decode_write_upsert() {
        let json = r#"{"type":"write","table":"tasks","op":"upsert","pk":"1","payload":{"title":"x"},"client_write_id":"w1"}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid write");
        let ClientMessage::Write {
            table,
            op,
            pk,
            payload,
            client_write_id,
        } = msg
        else {
            panic!("expected Write");
        };
        assert_eq!(table, "tasks");
        assert_eq!(op, "upsert");
        assert_eq!(pk, "1");
        assert_eq!(client_write_id, "w1");
        assert_eq!(payload, Some(serde_json::json!({"title": "x"})));
    }

    #[test]
    fn decode_write_delete_omits_payload() {
        // payload is optional (skip_serializing_if = "Option::is_none"); a
        // delete need not carry one.
        let json =
            r#"{"type":"write","table":"tasks","op":"delete","pk":"9","client_write_id":"w2"}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid write");
        let ClientMessage::Write {
            op, pk, payload, ..
        } = msg
        else {
            panic!("expected Write");
        };
        assert_eq!(op, "delete");
        assert_eq!(pk, "9");
        assert_eq!(payload, None);
    }

    #[test]
    fn decode_write_patch_carries_partial_payload() {
        // P3 parity: a patch carries ONLY the columns to change (not a full
        // row image). The codec treats it like an upsert payload (a JSON
        // object); the dispatch layer routes on `op`.
        let json = r#"{"type":"write","table":"tasks","op":"patch","pk":"1","payload":{"title":"x"},"client_write_id":"w3"}"#;
        let msg = decode_client_message(json.as_bytes()).expect("valid write");
        let ClientMessage::Write {
            table,
            op,
            pk,
            payload,
            client_write_id,
        } = msg
        else {
            panic!("expected Write");
        };
        assert_eq!(table, "tasks");
        assert_eq!(op, "patch");
        assert_eq!(pk, "1");
        assert_eq!(client_write_id, "w3");
        assert_eq!(payload, Some(serde_json::json!({"title": "x"})));
    }

    #[test]
    fn encode_write_result_ok_omits_error() {
        let bytes = encode_write_result("w1", true, None, true);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "write_result");
        assert_eq!(v["client_write_id"], "w1");
        assert_eq!(v["ok"], true);
        // error must be absent on the ok=true path.
        assert!(v.get("error").is_none() || v["error"].is_null());
    }

    #[test]
    fn encode_write_result_err_includes_message() {
        let bytes = encode_write_result("w9", false, Some("transient backend hiccup"), true);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "write_result");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "transient backend hiccup");
        // retryable is omitted when true — the old wire shape is preserved.
        assert!(v.get("retryable").is_none() || v["retryable"].is_null());
    }

    #[test]
    fn encode_write_result_permanent_marks_non_retryable() {
        let bytes = encode_write_result("w2", false, Some("table not writable: x"), false);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["retryable"], false);
    }

    #[test]
    fn encode_write_result_escapes_quotes_in_error() {
        // The error string is free-form adapter text; it must not break the
        // JSON. A double-quote + backslash round-trips cleanly.
        let bytes = encode_write_result("w\"", false, Some("bad \"col\\name\""), true);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "bad \"col\\name\"");
        assert_eq!(v["client_write_id"], "w\"");
    }

    // ---- snapshot-reconcile control frame tests (ADR-0014) ----

    #[test]
    fn encode_snapshot_boundary_begin_and_end() {
        let b = encode_snapshot_boundary("tasks", true);
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["type"], "snapshot_begin");
        assert_eq!(v["table"], "tasks");

        let e = encode_snapshot_boundary("users", false);
        let v: serde_json::Value = serde_json::from_slice(&e).unwrap();
        assert_eq!(v["type"], "snapshot_end");
        assert_eq!(v["table"], "users");
    }

    #[test]
    fn encode_snapshot_boundary_escapes_table_name() {
        // A table name with a quote must round-trip cleanly.
        let bytes = encode_snapshot_boundary("a\"b", true);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["table"], "a\"b");
    }

    #[test]
    fn encode_decode_resume_info_roundtrips() {
        let bytes = encode_resume_info(42, None);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "resume_info");
        assert_eq!(v["epoch"], 42);
        assert_eq!(decode_resume_info(&bytes), Some((42, None)));
    }

    #[test]
    fn decode_resume_info_rejects_non_resume_frames() {
        // ADR-0025 F2: the client intercept calls this BEFORE the row path, so
        // it must return None for everything that isn't a resume_info frame.
        assert_eq!(decode_resume_info(b""), None);
        assert_eq!(decode_resume_info(b"[]"), None);
        assert_eq!(decode_resume_info(b"{}"), None);
        assert_eq!(
            decode_resume_info(&encode_snapshot_boundary("tasks", true)),
            None
        );
        assert_eq!(
            decode_resume_info(br#"{"type":"resume_info"}"#),
            None,
            "no epoch"
        );
        assert_eq!(
            decode_resume_info(br#"{"type":"resume_info","epoch":"no"}"#),
            None,
            "non-numeric epoch"
        );
    }

    #[test]
    fn decode_control_frame_recognizes_begin_and_end() {
        let b = encode_snapshot_boundary("tasks", true);
        assert_eq!(decode_control_frame(&b), Some(("tasks".into(), true)));

        let e = encode_snapshot_boundary("tasks", false);
        assert_eq!(decode_control_frame(&e), Some(("tasks".into(), false)));
    }

    #[test]
    fn decode_control_frame_rejects_non_control_payloads() {
        // Replication frames (single object + array), write_results, garbage,
        // and empty payloads all yield None.
        let event = encode_event(&ev_n(7));
        assert!(decode_control_frame(&event).is_none());

        let batch = encode_events(&[&ev_n(1), &ev_n(2)]);
        assert!(decode_control_frame(&batch).is_none());

        let write_ack = encode_write_result("w1", true, None, true);
        assert!(decode_control_frame(&write_ack).is_none());

        assert!(decode_control_frame(b"not json").is_none());
        assert!(decode_control_frame(b"").is_none());
        assert!(decode_control_frame(b"   ").is_none());

        // A bare object with an unknown `type` is NOT a control frame.
        assert!(decode_control_frame(br#"{"type":"something_else","table":"t"}"#).is_none());
        // A snapshot_begin missing the `table` field is malformed → None.
        assert!(decode_control_frame(br#"{"type":"snapshot_begin"}"#).is_none());
    }

    // ---- P5 sync streams frames (docs/plans/p5-sync-streams-design.md §1) ----

    #[test]
    fn subscribe_stream_decodes_with_params() {
        let raw = br#"{"type":"subscribe_stream","id":"s1","stream":"lists","params":{"owner":"u1","min":3}}"#;
        let msg = decode_client_message(raw).expect("subscribe_stream decodes");
        match msg {
            ClientMessage::SubscribeStream { id, stream, params } => {
                assert_eq!(id, "s1");
                assert_eq!(stream, "lists");
                assert_eq!(params["owner"], serde_json::json!("u1"));
                assert_eq!(params["min"], serde_json::json!(3));
            }
            other => panic!("expected SubscribeStream, got {other:?}"),
        }
    }

    #[test]
    fn subscribe_stream_without_params_defaults_to_empty() {
        let raw = br#"{"type":"subscribe_stream","id":"s1","stream":"lists"}"#;
        let msg = decode_client_message(raw).expect("subscribe_stream decodes");
        match msg {
            ClientMessage::SubscribeStream { params, .. } => assert!(params.is_empty()),
            other => panic!("expected SubscribeStream, got {other:?}"),
        }
    }

    #[test]
    fn unsubscribe_stream_decodes() {
        let raw = br#"{"type":"unsubscribe_stream","id":"s1"}"#;
        let msg = decode_client_message(raw).expect("unsubscribe_stream decodes");
        match msg {
            ClientMessage::UnsubscribeStream { id } => assert_eq!(id, "s1"),
            other => panic!("expected UnsubscribeStream, got {other:?}"),
        }
    }

    #[test]
    fn stream_error_frame_encodes_the_documented_shape() {
        let bytes = encode_stream_error("s1", "unknown stream 'lists'");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "stream_error");
        assert_eq!(v["id"], "s1");
        assert_eq!(v["error"], "unknown stream 'lists'");
        // A stream_error is NOT a snapshot boundary — the control-frame
        // decoder must ignore it (it shares the same server_frames_tx pipe).
        assert!(decode_control_frame(&bytes).is_none());
        assert!(decode_snapshot_boundary(&bytes).is_none());
    }

    #[test]
    fn stream_error_escapes_free_form_strings() {
        let bytes = encode_stream_error("s\"1", "bad \"params\" for :owner");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "s\"1");
        assert_eq!(v["error"], "bad \"params\" for :owner");
    }

    #[test]
    fn resync_required_round_trips_and_rejects_other_shapes() {
        let bytes = encode_resync_required("tasks", "capacity shed detected");
        let (table, reason) = decode_resync_required(&bytes).expect("round trip");
        assert_eq!(table, "tasks");
        assert_eq!(reason, "capacity shed detected");
        // Not resync frames:
        assert!(decode_resync_required(b"not json").is_none());
        assert!(decode_resync_required(b"{\"type\":\"stream_error\"}").is_none());
    }

    #[test]
    fn stream_targeted_snapshot_boundary_round_trips() {
        let begin = encode_snapshot_boundary_for_stream("lists", "s1", true);
        assert_eq!(
            decode_snapshot_boundary(&begin),
            Some(("lists".to_string(), Some("s1".to_string()), true))
        );
        let end = encode_snapshot_boundary_for_stream("lists", "s1", false);
        assert_eq!(
            decode_snapshot_boundary(&end),
            Some(("lists".to_string(), Some("s1".to_string()), false))
        );
        // The pre-P5 decoder keeps working on the same bytes and simply
        // drops the stream id (old clients stay safe, design §1).
        assert_eq!(
            decode_control_frame(&begin),
            Some(("lists".to_string(), true))
        );
    }

    #[test]
    fn table_level_snapshot_boundary_has_no_stream_key() {
        let bytes = encode_snapshot_boundary("tasks", true);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("stream").is_none());
        assert_eq!(
            decode_snapshot_boundary(&bytes),
            Some(("tasks".to_string(), None, true))
        );
        assert_eq!(
            decode_control_frame(&bytes),
            Some(("tasks".to_string(), true))
        );
    }

    #[test]
    fn snapshot_boundary_ignores_unknown_keys_forward_compat() {
        // A future frame with extra keys still decodes (design §1: old
        // clients are safe against newer servers).
        let raw = br#"{"type":"snapshot_begin","table":"tasks","stream":"s1","future":42}"#;
        assert_eq!(
            decode_snapshot_boundary(raw),
            Some(("tasks".to_string(), Some("s1".to_string()), true))
        );
    }

    #[test]
    fn stream_error_decode_round_trips_and_rejects_other_shapes() {
        let bytes = encode_stream_error("s1", "unknown stream 'lists'");
        assert_eq!(
            decode_stream_error(&bytes),
            Some(("s1".to_string(), "unknown stream 'lists'".to_string()))
        );
        // Not a stream error: row frames, boundaries, write acks, garbage.
        let event = encode_event(&ev_n(7));
        assert!(decode_stream_error(&event).is_none());
        let boundary = encode_snapshot_boundary("tasks", true);
        assert!(decode_stream_error(&boundary).is_none());
        assert!(decode_stream_error(b"not json").is_none());
        assert!(decode_stream_error(br#"{"type":"stream_error","id":"s1"}"#).is_none());
    }
}
