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
/// socket carries the initial subscription, subsequent ACKs, and (future)
/// predicate updates. This is the inbound decode path that didn't exist before
/// T0-4 — `wire.rs` was encode-only.
///
/// - `subscribe` — the first frame: what to receive + where to resume from.
/// - `ack` — the client confirms it has applied through `lsn`; drives the
///   ack-driven slot advance (ADR-0009).
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
}
