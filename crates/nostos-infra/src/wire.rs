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

/// One frame on the wire. A client receives a stream of these. Reuses
/// [`Operation`] directly (it already serializes to `insert`/`update`/`delete`).
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

/// Stateless encoder. Usable concurrently from many tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct WireCodec;

impl WireCodec {
    /// Encode an event to a JSON frame (as bytes). Used by the server transport.
    #[must_use]
    pub fn encode(&self, event: &ReplicationEvent) -> Vec<u8> {
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
        let codec = WireCodec;
        let bytes = codec.encode(&ev());
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
        let codec = WireCodec;
        let del = ReplicationEvent::new(
            Lsn::new(1),
            RowOp::Delete {
                table: "tasks".into(),
                pk: "9".into(),
            },
        );
        let bytes = codec.encode(&del);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("payload").is_none() || v["payload"].is_null());
        assert_eq!(v["op"], "delete");
    }
}
