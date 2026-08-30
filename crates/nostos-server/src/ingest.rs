//! `POST /ingest` — the mirror write path (B2; arxa
//! docs/plans/doorbell-decision-2026-08-29.md section 4; ADR-0042).
//!
//! The studio engine is the SINGLE WRITER of the mirrored read-model; this
//! route is its only door. It is deliberately shaped like `PUT /rules`
//! (ADR-0031 Task 21): `NOSTOS_ADMIN_TOKEN` gates it fail-closed (unset -> a
//! literal 404 before headers or body are touched), bearer auth is
//! constant-time, and the auth check runs BEFORE any body parsing so an
//! unauthenticated caller learns nothing. The engine-side writer is
//! localhost-only (the sidecar binds the loopback), same posture as the
//! engine -> pushd doorbell POST.
//!
//! The tested core is [`apply_ingest`] (validate the whole batch, then apply
//! it — a malformed event applies NOTHING); the axum wrapper
//! [`post_ingest`] is the thin gate, mirroring how `put_rules_handler`
//! delegates to `apply_put_rules`.
//!
//! Batch semantics: all-or-nothing VALIDATION, then application. Application
//! cannot fail (an in-memory buffer + an unbounded channel), so a validated
//! batch is applied atomically in practice. The engine's mirror-out is
//! idempotent upserts keyed by row id, so a redelivered batch is a no-op by
//! construction — no client_write_id machinery needed.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nostos_domain::RowOp;
use nostos_infra::MirrorHandle;
use serde::Deserialize;
use serde_json::Value;

/// Router state for `/ingest`: the ingest half of the mirror. Constructed
/// by the composition root ONLY under `NOSTOS_REPLICATOR=mirror` (the route
/// is not mounted in other modes — there is no buffer to write into).
#[derive(Clone)]
pub struct IngestState {
    pub handle: MirrorHandle,
}

/// One mirrored row operation. `upsert` carries the full row image (the
/// tuple-image shape the read path delivers; its `"id"` column IS the pk —
/// the v1 convention everywhere else in the codebase, so the writer cannot
/// diverge pk from payload). `delete` carries only the pk.
#[derive(Debug, Deserialize)]
pub struct IngestEvent {
    pub table: String,
    /// "upsert" | "delete" — strict; the writer is ours, so typos should
    /// fail loudly instead of guessing.
    pub op: String,
    #[serde(default)]
    pub row: Option<Value>,
    #[serde(default)]
    pub pk: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IngestRequest {
    pub events: Vec<IngestEvent>,
}

/// A 400 with the offending index in the message — the single rejection
/// shape of [`validate_batch`].
fn bad(i: usize, msg: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": format!("events[{i}]: {msg}") })),
    )
}

/// Validate the whole batch and turn it into row operations. LSN stamping
/// happens in [`MirrorHandle::ingest`] (the single allocator), not here.
fn validate_batch(events: &[IngestEvent]) -> Result<Vec<RowOp>, (StatusCode, Json<Value>)> {
    let mut ops = Vec::with_capacity(events.len());
    for (i, event) in events.iter().enumerate() {
        if let Err(ident) = nostos_infra::ident::validate_ident(&event.table) {
            return Err(bad(i, &format!("invalid table identifier: {ident}")));
        }
        match event.op.as_str() {
            "upsert" => {
                let Some(row) = &event.row else {
                    return Err(bad(i, "op upsert requires a row"));
                };
                let Some(obj) = row.as_object() else {
                    return Err(bad(i, "row must be a JSON object"));
                };
                let Some(id) = obj.get("id").and_then(Value::as_str) else {
                    return Err(bad(i, "row must carry a string 'id' column (the pk)"));
                };
                if id.is_empty() {
                    return Err(bad(i, "row 'id' must be non-empty"));
                }
                if event.pk.is_some() {
                    return Err(bad(
                        i,
                        "pk is only valid with op delete (the row's id is the pk)",
                    ));
                }
                ops.push(RowOp::Insert {
                    table: event.table.clone(),
                    pk: id.to_string(),
                    payload: bytes::Bytes::from(
                        serde_json::to_vec(row).expect("a Value is always serializable"),
                    ),
                });
            }
            "delete" => {
                let Some(pk) = &event.pk else {
                    return Err(bad(i, "op delete requires a pk"));
                };
                if event.row.is_some() {
                    return Err(bad(i, "row is only valid with op upsert"));
                }
                ops.push(RowOp::Delete {
                    table: event.table.clone(),
                    pk: pk.clone(),
                    old_payload: None,
                });
            }
            other => return Err(bad(i, &format!("unknown op: {other}"))),
        }
    }
    Ok(ops)
}

/// The tested core: validate the whole batch, apply every event, echo the
/// server-stamped LSNs (the writer correlates its mirror cursor against
/// them). An invalid batch applies NOTHING.
pub fn apply_ingest(
    handle: &MirrorHandle,
    raw_body: &[u8],
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let body: IngestRequest = serde_json::from_slice(raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid JSON body: {e}") })),
        )
    })?;
    let ops = validate_batch(&body.events)?;
    let mut lsns = Vec::with_capacity(ops.len());
    for op in ops {
        let event = handle.ingest(op);
        lsns.push(event.lsn.0);
    }
    Ok(Json(
        serde_json::json!({ "accepted": lsns.len(), "lsns": lsns }),
    ))
}

/// The axum gate: same ordering as `put_rules_handler` — 404 (token unset,
/// route "not mounted") before 401 before Content-Type before parsing.
pub async fn post_ingest(
    State(state): State<IngestState>,
    headers: HeaderMap,
    raw_body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(admin_token) = crate::admin_auth::admin_token_from_env() else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        ));
    };
    if !crate::admin_auth::AdminAuth::check(&headers, &admin_token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        ));
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(serde_json::json!({ "error": "Content-Type must be application/json" })),
        ));
    }
    apply_ingest(&state.handle, &raw_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(value: &Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("json value serializes")
    }

    fn upsert_batch(table: &str, id: &str) -> Vec<u8> {
        body_of(&serde_json::json!({ "events": [
            { "table": table, "op": "upsert", "row": { "id": id } }
        ] }))
    }

    #[test]
    fn happy_batch_applies_and_echoes_monotonic_lsns() {
        let (handle, _replicator) = MirrorHandle::open();
        let body = body_of(&serde_json::json!({ "events": [
            { "table": "approvals", "op": "upsert",
              "row": { "id": "a1", "status": "pending" } },
            { "table": "approvals", "op": "delete", "pk": "a2" }
        ] }));
        let resp = apply_ingest(&handle, &body).unwrap();
        assert_eq!(resp.0.get("accepted").and_then(Value::as_u64), Some(2));
        let lsns = resp.0.get("lsns").and_then(Value::as_array).unwrap();
        assert_eq!(lsns.len(), 2);
        assert!(lsns[0].as_u64() < lsns[1].as_u64());
        let rows = handle.buffered_rows("approvals");
        assert_eq!(rows.len(), 1, "upsert applied, delete removed");
        assert_eq!(rows[0].0, "a1");
    }

    #[test]
    fn invalid_batch_applies_nothing() {
        let (handle, _replicator) = MirrorHandle::open();
        let body = body_of(&serde_json::json!({ "events": [
            { "table": "approvals", "op": "upsert", "row": { "id": "ok" } },
            { "table": "bad; DROP TABLE x", "op": "upsert", "row": { "id": "no" } }
        ] }));
        let err = apply_ingest(&handle, &body).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            handle.buffered_rows("approvals").is_empty(),
            "all-or-nothing: the valid first event must NOT be applied",
        );
    }

    #[test]
    fn upsert_row_without_string_id_is_rejected() {
        let (handle, _replicator) = MirrorHandle::open();
        let body = body_of(&serde_json::json!({ "events": [
            { "table": "approvals", "op": "upsert", "row": { "no_id": true } }
        ] }));
        let err = apply_ingest(&handle, &body).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn delete_requires_pk_and_upsert_rejects_pk() {
        let (handle, _replicator) = MirrorHandle::open();
        let err = apply_ingest(
            &handle,
            &body_of(&serde_json::json!({ "events": [
                { "table": "approvals", "op": "delete", "row": { "id": "x" } }
            ] })),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let err = apply_ingest(
            &handle,
            &body_of(&serde_json::json!({ "events": [
                { "table": "approvals", "op": "upsert", "pk": "x", "row": { "id": "x" } }
            ] })),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unknown_op_is_rejected() {
        let (handle, _replicator) = MirrorHandle::open();
        let err = apply_ingest(
            &handle,
            &body_of(&serde_json::json!({ "events": [
                { "table": "approvals", "op": "patch", "pk": "x" }
            ] })),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn malformed_json_is_a_400() {
        let (handle, _replicator) = MirrorHandle::open();
        let err = apply_ingest(&handle, br#"{"events": ["#).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn lsns_advance_across_batches() {
        let (handle, _replicator) = MirrorHandle::open();
        let first = apply_ingest(&handle, &upsert_batch("approvals", "a1")).unwrap();
        let second = apply_ingest(&handle, &upsert_batch("approvals", "a2")).unwrap();
        let l1 = first.0["lsns"][0].as_u64().unwrap();
        let l2 = second.0["lsns"][0].as_u64().unwrap();
        assert!(l2 > l1);
    }
}
