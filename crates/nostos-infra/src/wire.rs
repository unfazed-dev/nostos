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
        /// `"upsert"` or `"delete"`.
        op: String,
        /// Primary-key value (v1 convention: pk column is `id`).
        pk: String,
        /// The row image for an upsert: a JSON object of column → value, the
        /// same tuple-image shape the read path delivers. Absent (`None`) for
        /// deletes. A non-object (array / scalar / null) is rejected as
        /// `InvalidPayload` before any SQL is built.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
        /// Client-supplied correlation id, echoed back in the `WriteResult`.
        /// Lets the client match each ack to its request (the round-trip is
        /// asynchronous: the write task may be behind the client's send loop).
        client_write_id: String,
    },
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
        RowOp::Delete { table, pk } => (Operation::Delete, table.clone(), pk.clone(), None),
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
#[must_use]
pub fn encode_write_result(client_write_id: &str, ok: bool, error: Option<&str>) -> Vec<u8> {
    // Hand-built JSON keeps this allocation-light and avoids inventing a struct
    // for one outbound shape. The fields are simple (string/bool), so escaping
    // the two free-form strings (id + error) is the only care needed.
    let mut out = String::with_capacity(64 + client_write_id.len());
    out.push_str("{\"type\":\"write_result\",\"client_write_id\":");
    push_json_string(&mut out, client_write_id);
    out.push_str(",\"ok\":");
    out.push_str(if ok { "true" } else { "false" });
    if let Some(err) = error {
        out.push_str(",\"error\":");
        push_json_string(&mut out, err);
    }
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
        RowOp::Delete { table, pk } => (Operation::Delete, table.clone(), pk.clone(), None),
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
        } = msg
        else {
            panic!("expected Subscribe");
        };
        assert_eq!(table, "users");
        assert!(filters.is_empty());
        assert_eq!(where_sql, None);
        assert_eq!(resume_lsn, None);
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
    fn encode_write_result_ok_omits_error() {
        let bytes = encode_write_result("w1", true, None);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "write_result");
        assert_eq!(v["client_write_id"], "w1");
        assert_eq!(v["ok"], true);
        // error must be absent on the ok=true path.
        assert!(v.get("error").is_none() || v["error"].is_null());
    }

    #[test]
    fn encode_write_result_err_includes_message() {
        let bytes = encode_write_result("w9", false, Some("table not writable: x"));
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "write_result");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "table not writable: x");
    }

    #[test]
    fn encode_write_result_escapes_quotes_in_error() {
        // The error string is free-form adapter text; it must not break the
        // JSON. A double-quote + backslash round-trips cleanly.
        let bytes = encode_write_result("w\"", false, Some("bad \"col\\name\""));
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "bad \"col\\name\"");
        assert_eq!(v["client_write_id"], "w\"");
    }
}
