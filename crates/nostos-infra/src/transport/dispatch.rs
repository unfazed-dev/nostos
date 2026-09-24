use crate::router::TokioEventSink;
use crate::wire::{encode_write_result, ClientMessage};
use nostos_application::ports::{WriteBack, WriteBackError};
use nostos_domain::Principal;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::debug;

/// Apply a decoded inbound Ack/Write client message. `Subscribe` is routed by
/// the reader task to `register_subscribe` (this handler never sees it in the
/// current flow, but stays defensive). The Write body is the ADR-0013 trust
/// boundary: allowlist FIRST, then tenant-scoped dispatch to the write-back
/// port, then a `WriteResult` ack queued to the writer task. The write call is
/// tenant-scoped exactly like the read path (ADR-0018) —
/// `principal.tenant_scope(tenant_column)` is the same seam `build_predicate`
/// uses, so read/write enforcement can't drift.
pub(super) async fn handle_decoded_message(
    msg: ClientMessage,
    sink: &TokioEventSink,
    write_back: &Arc<dyn WriteBack>,
    allowlist: &HashSet<String>,
    principal: &Principal,
    tenant_column: Option<&str>,
    server_frames_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    match msg {
        ClientMessage::Ack { lsn } => {
            sink.record_ack(nostos_domain::Lsn::new(lsn));
            debug!(ack_lsn = lsn, "client acknowledged progress");
        }
        ClientMessage::Write {
            table,
            op,
            pk,
            payload,
            client_write_id,
        } => {
            // ALLOWLIST FIRST (ADR-0013 trust boundary). The transport enforces
            // the table allowlist before the adapter is ever called, so this is
            // one uniform gate that holds regardless of adapter. The
            // `PgWriteBack` adapter re-validates it as defense-in-depth.
            if !allowlist.contains(&table) {
                // Actionable rejection (ADR-0013). The empty-default (no tables
                // writable) is deliberate — defense-in-depth at the SQL-injection
                // trust boundary — so name the table + the exact env var that
                // opens it, teaching the model instead of failing silently. The
                // `"table not writable"` prefix is asserted by ws_contract.rs.
                let msg = format!(
                    "table not writable: '{table}' — add it to NOSTOS_WRITE_TABLES \
                     (env, comma-separated; e.g. NOSTOS_WRITE_TABLES={table}). \
                     Empty by default = no tables writable (ADR-0013)."
                );
                // Permanent: no retry can put the table on the allowlist mid-
                // session — flag non-retryable so the client dead-letters on the
                // first rejection (ADR-0013 v2 transient-vs-permanent).
                let frame = encode_write_result(&client_write_id, false, Some(&msg), false);
                let _ = server_frames_tx.try_send(frame);
                debug!(table = %table, "write rejected: table not writable");
                return;
            }
            // ADR-0018: the tenant scope, if enforcement is active, travels
            // with the write so the adapter can force-stamp/guard by it —
            // never trust anything the client sent for the tenant column.
            let tenant = principal.tenant_scope(tenant_column);
            // Dispatch to the write-back port. The result is reported back to
            // the client as a WriteResult frame; the written row then flows
            // out through normal replication to every subscriber.
            let result =
                dispatch_write(write_back, &table, &op, &pk, payload.as_ref(), tenant).await;
            // Classify before the frame: allowlist + payload-shape failures are
            // client-controlled inputs no retry can fix — non-retryable, so the
            // client quarantines immediately instead of cycling the write
            // through the full attempt budget. Backend errors stay retryable.
            let (ok, error, retryable) = match result {
                Ok(()) => (true, None, true),
                Err(
                    e @ (WriteBackError::TableNotAllowed(_) | WriteBackError::InvalidPayload(_)),
                ) => (false, Some(e.to_string()), false),
                Err(e) => (false, Some(e.to_string()), true),
            };
            let frame = encode_write_result(&client_write_id, ok, error.as_deref(), retryable);
            // If the channel is full (client disconnected / backpressure), the
            // ack is dropped — the writer loop will end on the next failed
            // send anyway. Best-effort; not fatal.
            let _ = server_frames_tx.try_send(frame);
            if let Some(err) = error.as_deref() {
                // Rejections are logged loud with the reason: a silent
                // ok=false is undiagnosable from the server side (the reason
                // otherwise travels only in the WriteResult frame).
                tracing::warn!(
                    table = %table,
                    op = %op,
                    error = %err,
                    "write rejected"
                );
            } else {
                debug!(
                    table = %table,
                    op = %op,
                    ok,
                    "write applied — WriteResult queued"
                );
            }
        }
        // Subscribe is routed by the reader to `register_subscribe`; reaching
        // here is impossible in the current flow, but stay defensive.
        ClientMessage::Subscribe { .. } => {
            debug!("subscribe reached decoded-message handler");
        }
        // P5 sync streams: the reader routes both stream frames to
        // `register_stream`/`unregister_stream`; these arms are the defensive
        // fallback for any future call site — no-op pass-through (never reject).
        ClientMessage::SubscribeStream { .. } | ClientMessage::UnsubscribeStream { .. } => {
            // No-op: the reader loop handles these; this arm exists only for
            // defensive completeness if `handle_decoded_message` is called
            // directly in future refactoring.
        }
    }
}

/// Translate a `Write` client message into a `WriteBack` port call. The op
/// string is `"upsert" | "delete" | "patch" | "increment"`; anything else is an
/// `InvalidPayload`. The payload (a `serde_json::Value`) is rendered back to
/// JSON text for the upsert/patch/increment paths (the port takes `&str`).
/// `tenant` (ADR-0018) is forwarded verbatim to the adapter — `dispatch_write`
/// doesn't interpret it, just relays the scope the caller already computed
/// from the principal.
async fn dispatch_write(
    write_back: &Arc<dyn WriteBack>,
    table: &str,
    op: &str,
    pk: &str,
    payload: Option<&serde_json::Value>,
    tenant: Option<nostos_domain::TenantScope<'_>>,
) -> Result<(), WriteBackError> {
    match op {
        "upsert" => {
            // The payload must be present and a JSON object for an upsert. A
            // missing/non-object payload is InvalidPayload. The adapter ALSO
            // validates the object-ness, but we catch it here too so the error
            // is surfaced uniformly.
            let value = payload.ok_or_else(|| {
                WriteBackError::InvalidPayload("payload required for upsert".into())
            })?;
            if !value.is_object() {
                return Err(WriteBackError::InvalidPayload(
                    "payload must be a JSON object".into(),
                ));
            }
            let json = value.to_string();
            write_back.upsert(table, pk, &json, tenant).await
        }
        "patch" => {
            // A patch carries the partial column set (same object shape as an
            // upsert payload). Same object-ness guard as upsert — the adapter
            // re-validates too.
            let value = payload.ok_or_else(|| {
                WriteBackError::InvalidPayload("payload required for patch".into())
            })?;
            if !value.is_object() {
                return Err(WriteBackError::InvalidPayload(
                    "payload must be a JSON object".into(),
                ));
            }
            let json = value.to_string();
            write_back.patch(table, pk, &json, tenant).await
        }
        "increment" => {
            // ADR-0030 Decision 1: server-authoritative counter delta. Payload
            // is `{"field","delta"}`; PgWriteBack emits SET col = col + ?. Same
            // object-ness guard — the adapter re-validates field/delta.
            let value = payload.ok_or_else(|| {
                WriteBackError::InvalidPayload("payload required for increment".into())
            })?;
            if !value.is_object() {
                return Err(WriteBackError::InvalidPayload(
                    "payload must be a JSON object".into(),
                ));
            }
            let json = value.to_string();
            write_back.increment(table, pk, &json, tenant).await
        }
        "delete" => write_back.delete(table, pk, tenant).await,
        other => Err(WriteBackError::InvalidPayload(format!(
            "unknown op: {other} (expected 'upsert', 'delete', 'patch', or 'increment')"
        ))),
    }
}
