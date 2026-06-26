//! Wire codec — translate between `ReplicationEvent` and on-the-wire frames.
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

/// One frame on the wire. A client receives a stream of these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireFrame {
    pub lsn: u64,
    pub op: WireOp,
    pub table: String,
    pub pk: String,
    /// Hex-encoded payload (omitted for deletes to keep frames small).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub txn_id: Option<u64>,
}

/// Operation tag on the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WireOp {
    Insert,
    Update,
    Delete,
}

impl From<Operation> for WireOp {
    fn from(o: Operation) -> Self {
        match o {
            Operation::Insert => Self::Insert,
            Operation::Update => Self::Update,
            Operation::Delete => Self::Delete,
        }
    }
}

/// Encode/decode frames. Stateless — usable concurrently from many tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct WireCodec;

impl WireCodec {
    /// Encode an event to a JSON frame (as bytes). Used by the server transport.
    #[must_use]
    pub fn encode(&self, event: &ReplicationEvent) -> Vec<u8> {
        let frame = self.to_frame(event);
        // Unwrap is safe: our types are all JSON-serializable.
        serde_json::to_vec(&frame).expect("wire frame must serialize")
    }

    /// Decode bytes back into a frame. Used by the benchmark client.
    ///
    /// Returns `None` on malformed input rather than propagating an error —
    /// the benchmark treats a bad frame as a protocol bug and panics higher up.
    #[must_use]
    pub fn decode(&self, bytes: &[u8]) -> Option<WireFrame> {
        serde_json::from_slice(bytes).ok()
    }

    /// Project an event to its wire representation.
    #[must_use]
    pub fn to_frame(&self, event: &ReplicationEvent) -> WireFrame {
        let (op, table, pk, payload) = match &event.op {
            RowOp::Insert { table, pk, payload } => (
                WireOp::Insert,
                table.clone(),
                pk.clone(),
                Some(hex::encode(payload)),
            ),
            RowOp::Update { table, pk, payload } => (
                WireOp::Update,
                table.clone(),
                pk.clone(),
                Some(hex::encode(payload)),
            ),
            RowOp::Delete { table, pk } => (WireOp::Delete, table.clone(), pk.clone(), None),
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
}

/// Tiny hex encoder — avoids pulling a `hex` crate for one fn.
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

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

    use nostos_domain::Lsn;

    #[test]
    fn encode_decode_roundtrip() {
        let codec = WireCodec;
        let original = ev();
        let bytes = codec.encode(&original);
        let frame = codec.decode(&bytes).expect("must decode");
        assert_eq!(frame.lsn, 42);
        assert_eq!(frame.op, WireOp::Insert);
        assert_eq!(frame.table, "tasks");
        assert_eq!(frame.pk, "1");
        assert_eq!(frame.txn_id, Some(7));
        // payload hex of b"hi" == "6869"
        assert_eq!(frame.payload.as_deref(), Some("6869"));
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
        let frame = codec.to_frame(&del);
        assert!(frame.payload.is_none());
        assert_eq!(frame.op, WireOp::Delete);
    }

    #[test]
    fn malformed_decodes_to_none() {
        let codec = WireCodec;
        assert!(codec.decode(b"not json").is_none());
    }
}
