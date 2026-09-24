use super::dispatch::handle_decoded_message;
use super::state::SyncRouterState;
use super::subscribe::{
    register_stream, register_subscribe, resume_advertisement, unregister_stream, SocketSubs,
    SubscribeReject,
};
use super::{DEFAULT_SESSION_BUFFER, MAX_BATCH_FRAMES, RULES_CHANGED_CLOSE_REASON};
use crate::router::TokioEventSink;
use crate::wire::{
    decode_client_message, encode_event, encode_events, encode_resume_info, encode_resync_required,
    encode_stream_error, ClientMessage,
};
use axum::extract::ws::Message;
use futures_util::stream::StreamExt as _;
use nostos_domain::{Principal, ReplicationEvent, SessionId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, warn};

/// Drive one WebSocket connection for its lifetime.
///
/// Generic over the frame transport (ADR-0041 D6): the axum path passes its
/// upgraded `WebSocket` directly; the iroh accept loop passes a tungstenite
/// `WebSocketStream` over a QUIC bi-stream behind the message adapter in
/// `crate::iroh_sync`. Both are one `Stream + Sink` of axum `Message`s, so
/// everything below the split is byte-identical per transport.
pub(crate) async fn run_session<S, E>(
    mut socket: S,
    state: SyncRouterState,
    principal: Principal,
    exp: Option<i64>,
) where
    S: futures_util::Stream<Item = Result<Message, E>>
        + futures_util::Sink<Message, Error = E>
        + Unpin
        + Send
        + 'static,
    E: Send + 'static,
{
    // 1. Read the subscribe frame.
    let Some(subscribe) = read_subscribe(&mut socket).await else {
        return; // client disconnected without subscribing
    };

    // 2. Allocate the ONE shared sink for this socket. N tables deliver into
    //    this one bounded channel; a single writer task drains it onto the wire.
    //    We keep the *concrete* `Arc<TokioEventSink>` for close()/record_ack()
    //    + snapshot delivery; `register_subscribe` derives the type-erased
    //    `Arc<dyn EventSink>` clone the store holds per registered session.
    let (sink, mut rx) = TokioEventSink::channel(state.session_buffer);
    let sink_concrete = Arc::new(sink);

    // 3. Seed the resume cursor ONCE from the first subscribe (the client's
    //    global checkpoint). The socket's `synthetic_cursor` (the snapshot LSN
    //    allocator) derives from the same value; per-frame resume_lsn on later
    //    subscribes is ignored, so a mid-stream snapshot can't be dropped past
    //    an already-advanced checkpoint.
    if let Some(resume) = subscribe.resume_lsn {
        sink_concrete.seed_acked_lsn(nostos_domain::Lsn::new(resume));
        debug!(
            resume_lsn = resume,
            "session resuming from client checkpoint"
        );
    }

    // Clone the principal + tenant for BOTH the write path and the read-side
    // subscribe path (ADR-0018): the read path (predicate injection) and the
    // write path (tenant-scoped stamping/guards) share one authenticated
    // identity. `principal` is borrowed for the first register_subscribe
    // below; the clones live on in the reader task.
    let write_principal = principal.clone();
    let tenant_column_for_writes = state.tenant_column.clone();

    let manager = Arc::clone(&state.manager);
    let snapshotter = state.snapshotter.clone();
    // ADR-0031: snapshot the active ruleset once per socket. Cloned (cheap —
    // wraps a BTreeMap) rather than holding the RwLock read guard across the
    // awaits inside register_subscribe. This snapshot becomes `old_ruleset`
    // in the write_loop below (D3, Task 14) so a live rules reload can be
    // verified against exactly what this socket's already-registered
    // subscriptions were granted.
    let ruleset = state.rules.read().await.clone();

    // 4. Per-socket multi-table state. `synthetic_cursor` is seeded from the
    //    first subscribe's resume_lsn (0 for a fresh client) and advanced by
    //    each snapshot's row count — the load-bearing fix that keeps multi-
    //    table snapshot-on-subscribe correct on a shared sink (see
    //    `register_subscribe` + ADR-0022).
    let subs = Arc::new(Mutex::new(SocketSubs {
        ids: Vec::new(),
        tables: HashSet::new(),
        synthetic_cursor: subscribe.resume_lsn.unwrap_or(0),
        streams: HashMap::new(),
    }));

    // Pre-encoded control frame channel (write_result acks + snapshot-reconcile
    // boundaries — ADR-0013 v2 + ADR-0014). Created BEFORE the first
    // `register_subscribe` so the first-table snapshot can emit a begin/end
    // pair through it; the writer task (split below) drains the rx half.
    let (server_frames_tx, mut server_frames_rx) =
        tokio::sync::mpsc::channel::<Vec<u8>>(DEFAULT_SESSION_BUFFER);

    // ADR-0025 F2: advertise the server's current slot epoch ONCE at subscribe
    // (before snapshot/replay frames) on BOTH paths, so the client can persist
    // + resend it on reconnect (the resume gate compares client vs server
    // epoch). Read fresh here — register_subscribe reads the same raw value
    // below for the gate, so the client persists exactly what its resume will
    // be judged against.
    let server_epoch = state
        .metrics
        .slot_epoch
        .load(std::sync::atomic::Ordering::Relaxed);
    // ADR-0031 D2: a client that sent `rules_checksum` on its Subscribe frame
    // gets the raw epoch + the raw checksum advertised back, so its logs can
    // tell "slot recreated" from "rules changed". A pre-D2 client (no
    // `rules_checksum` in its Subscribe) gets the old composed value — its
    // frame is byte-identical to today's.
    let has_checksum = subscribe.client_rules_checksum.is_some();
    if !has_checksum {
        debug!("client omitted rules_checksum; using composed-epoch fallback");
    }
    let (advertised_epoch, advertised_checksum) =
        resume_advertisement(has_checksum, server_epoch, ruleset.checksum());
    let _ = server_frames_tx
        .send(encode_resume_info(advertised_epoch, advertised_checksum))
        .await;

    // 6. Split the socket: writer drains the shared sink, reader parses ACK/
    //    Write frames AND handles additional Subscribe frames (registering more
    //    tables on the SAME sink). Same single-writer serialization as before:
    //    events AND write-acks share one socket sink, no interleaving race.
    let (writer, mut reader) = socket.split();

    let closed = Arc::new(Notify::new());
    let closed_tx = Arc::clone(&closed);
    let ack_sink = Arc::clone(&sink_concrete);

    // ADR-0029 §Decision-4 (live-socket): a live socket must not outlive its
    // JWT. Arm a one-shot deadline at `exp + leeway`; when it fires the writer
    // (below) sends Close(4401) and ends the session — the client's reconnect
    // loop then reconnects with the refreshed token held via `set_token`. Armed
    // only when the handshake token carried an `exp` (None ⇒ no-op: the OSS
    // `sync_auth: none` default and no-`exp` tokens behave exactly as before).
    let exp_fired = Arc::new(Notify::new());
    let exp_task = exp.map(|exp_secs| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(i64::MAX, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let remaining = u64::try_from(
            (exp_secs + crate::auth::JWT_LEEWAY_SECS)
                .saturating_sub(now_secs)
                .max(0),
        )
        .unwrap_or(0);
        let notify = Arc::clone(&exp_fired);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
            notify.notify_one();
        })
    });
    let exp_for_writer = Arc::clone(&exp_fired);

    // ADR-0031 D3: live-session re-scoping on a rules reload. `rules_rx` only
    // wakes when the watcher (crates/nostos-server/src/main.rs::watch_rules)
    // observes an actual checksum change on the rules file — a
    // `watch::Receiver` that has already seen the current value never fires,
    // so this arm costs nothing on the per-event delivery path below until an
    // operator actually edits `nostos_rules.toml`.
    let mut rules_rx = state.rules_changed.clone();
    let rules_shared = Arc::clone(&state.rules);
    let subs_for_reload = Arc::clone(&subs);
    let principal_for_reload = principal.clone();
    // ADR-0040: the writer loop watches this sink's capacity-shed counter and
    // emits one `resync_required` per shed episode when the signal is on.
    let resync_sink = Arc::clone(&sink_concrete);
    let resync_enabled = state.resync_signal;
    let resync_subs = Arc::clone(&subs);
    let mut last_signaled_shed: u64 = 0;

    // ADR-0040 reorder: the writer owns the socket half, so first-register
    // rejections request their polite Close through this channel.
    let (close_tx, close_rx) = tokio::sync::mpsc::channel::<axum::extract::ws::CloseFrame>(1);
    let writer_ruleset = ruleset.clone();
    let write_loop = tokio::spawn(async move {
        use futures_util::sink::SinkExt as _;
        let mut writer = writer;
        let mut close_rx = close_rx;
        let mut old_ruleset = writer_ruleset;
        // C3 batched-writes: the first frame is awaited (no busy-spin, no
        // latency tax when idle). Once one is in hand, drain up to
        // `MAX_BATCH_FRAMES - 1` MORE frames that are *immediately available*
        // (non-blocking `try_recv`). If only the awaited frame is available,
        // send it as a single object — byte-identical to the pre-batching wire
        // (so the low-rate path adds zero cost and stays wire-compatible with
        // any client that only understands single-object messages). Only when
        // the channel has a backlog (≥2 pending) do we coalesce into one JSON
        // array message, amortizing N frame-encode + socket-send costs into
        // one wire write. The receiver decodes both forms (`decode_frames`).
        //
        // D2: the writer ALSO drains `server_frames_rx` (the reader's
        // WriteResult acks). We `select!` over both sources so neither starves
        // the other; a pending write-ack goes out promptly even under event
        // backlog (it's a single small frame, never batched with events —
        // `WriteResult` is its own wire shape, not a replication frame).
        loop {
            // ADR-0040 continuity signal (see captures above). Delta-based:
            // fires once per shed episode; the client clears + reconciles,
            // which tears this session down anyway.
            if resync_enabled {
                let sheds = resync_sink.capacity_sheds();
                if sheds != last_signaled_shed {
                    let tables = {
                        let s = resync_subs.lock().await;
                        s.tables.iter().cloned().collect::<Vec<_>>().join(",")
                    };
                    last_signaled_shed = sheds;
                    let frame = encode_resync_required(&tables, "capacity shed detected");
                    if writer.send(Message::Binary(frame)).await.is_err() {
                        break; // client gone
                    }
                }
            }
            // Await the next thing to send: an event batch OR a write-ack frame.
            tokio::select! {
                // Replication events from the fan-out sink.
                maybe_first = rx.recv() => {
                    let Some(first) = maybe_first else { break; };
                    // The sink channel carries Events AND Control frames (snapshot
                    // boundaries) on one FIFO queue (ADR-0025 hole #2). A Control
                    // goes out immediately, alone (a different wire shape — can't
                    // batch with events); an Event starts a batch. Draining stops
                    // at a Control so it keeps its FIFO slot, sent right after the
                    // batch it followed — that is what guarantees begin → rows →
                    // end on the wire.
                    match first {
                        crate::router::SinkMsg::Control(bytes) => {
                            if writer.send(Message::Binary(bytes)).await.is_err() {
                                break; // client gone
                            }
                        }
                        crate::router::SinkMsg::Event(first_ev) => {
                            let mut batch: Vec<Arc<ReplicationEvent>> =
                                Vec::with_capacity(MAX_BATCH_FRAMES);
                            batch.push(first_ev);
                            let mut pending_control: Option<Vec<u8>> = None;
                            while batch.len() < MAX_BATCH_FRAMES {
                                match rx.try_recv() {
                                    Ok(crate::router::SinkMsg::Event(ev)) => batch.push(ev),
                                    Ok(crate::router::SinkMsg::Control(bytes)) => {
                                        pending_control = Some(bytes);
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let msg = if batch.len() == 1 {
                                Message::Binary(encode_event(&batch[0]))
                            } else {
                                let refs: Vec<&ReplicationEvent> =
                                    batch.iter().map(|e| &**e).collect();
                                Message::Binary(encode_events(&refs))
                            };
                            if writer.send(msg).await.is_err() {
                                break; // client gone
                            }
                            if let Some(bytes) = pending_control {
                                if writer.send(Message::Binary(bytes)).await.is_err() {
                                    break; // client gone
                                }
                            }
                        }
                    }
                }
                // WriteResult acks from the reader task (D2). Never batched —
                // a write-ack is its own wire shape, sent immediately.
                maybe_ack = server_frames_rx.recv() => {
                    let Some(bytes) = maybe_ack else { break; };
                    if writer.send(Message::Binary(bytes)).await.is_err() {
                        break; // client gone
                    }
                }
                // ADR-0040: first-register (and cap) rejects arrive here as a
                // polite Close with the wire-contract reason.
                maybe_close = close_rx.recv() => {
                    if let Some(frame) = maybe_close {
                        let _ = writer.send(Message::Close(Some(frame))).await;
                    }
                    break;
                }
                // ADR-0029 §Decision-4 (live-socket): token-expiry deadline.
                // Inert (never notified) for no-`exp`/anonymous sessions.
                () = exp_for_writer.notified() => {
                    debug!("closing socket: token expired (ADR-0029 §Decision-4)");
                    let frame = axum::extract::ws::CloseFrame {
                        code: 4401,
                        reason: "nostos: token expired".into(),
                    };
                    let _ = writer
                        .send(axum::extract::ws::Message::Close(Some(frame)))
                        .await;
                    break;
                }
                // ADR-0031 D3: sync-rules reload. Verification is per-table:
                // only a subscribed table whose rule decision changed at all
                // trips this — an edit that never touches this socket's
                // tables is free.
                res = rules_rx.changed() => {
                    if res.is_err() {
                        // Sender dropped (shutdown / miswired with_rules):
                        // changed() resolves Err immediately FOREVER, so a
                        // continue here is a 100%-CPU busy-spin (audit L11).
                        break;
                    }
                    let new_ruleset = rules_shared.read().await.clone();
                    let narrowed = {
                        let s = subs_for_reload.lock().await;
                        let table_changed = s.tables.iter().any(|table| {
                            old_ruleset.decide(table, &principal_for_reload)
                                != new_ruleset.decide(table, &principal_for_reload)
                        });
                        // P5: a live stream is sensitive to (a) its table's
                        // rule decision and (b) its own template definition —
                        // a streams edit = a rules edit → resnapshot (design
                        // §2 checksum participation; after the reconnect the
                        // client re-subscribes its streams against fresh
                        // definitions). Same coarse close-the-socket ponytail
                        // as the table path below.
                        let stream_changed = s.streams.values().any(|sub| {
                            old_ruleset.stream(&sub.name) != new_ruleset.stream(&sub.name)
                                || old_ruleset.decide(&sub.table, &principal_for_reload)
                                    != new_ruleset.decide(&sub.table, &principal_for_reload)
                        });
                        table_changed || stream_changed
                    };
                    if narrowed {
                        debug!("closing socket: sync rules changed under a live session (ADR-0031 D3)");
                        // ponytail: swap verification is coarse — ANY per-table
                        // rule-decision change (a real narrow, or a widen this
                        // code can't prove safe) disconnects the whole socket
                        // rather than re-scoping just the affected subscription
                        // in place. Ceiling: one reconnect + resnapshot per
                        // connected client per rules edit that touches one of
                        // its subscribed tables, including edits that only
                        // widened. Upgrade path: a real subset/implication
                        // check on `PredicateExpr` for the Allow-to-Allow case
                        // so a genuine widen can keep running in place, plus
                        // per-subscription differential resync instead of
                        // closing the whole socket.
                        let frame = axum::extract::ws::CloseFrame {
                            code: axum::extract::ws::close_code::INVALID,
                            reason: RULES_CHANGED_CLOSE_REASON.into(),
                        };
                        let _ = writer
                            .send(axum::extract::ws::Message::Close(Some(frame)))
                            .await;
                        break;
                    }
                    old_ruleset = new_ruleset;
                }
            }
        }
        // Give the peer a moment to read a just-forwarded Close before the
        // socket drops out from under it (otherwise the client can observe
        // an empty-reason close — ADR-0040 reorder follow-up).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = writer;
        closed_tx.notify_waiters();
    });
    // 5. Register the FIRST table. A where_sql rejection or the global device
    //    cap is FATAL here (close the socket with a reason before any event
    //    flows, same as the single-table path); subsequent rejects are
    //    non-fatal (the reader logs + keeps serving existing subscriptions).
    if let Err(reject) = register_subscribe(
        &subscribe,
        &subs,
        &manager,
        snapshotter.as_ref(),
        server_epoch,
        state.oplog_reader.as_ref(),
        &sink_concrete,
        &principal,
        state.tenant_column.as_deref(),
        &ruleset,
    )
    .await
    {
        match reject {
            SubscribeReject::Rejected(reason) => {
                // Post-split the writer owns the socket half: request the
                // polite Close through it so the wire contract keeps the
                // rejection reason (asserted by ws_contract tests).
                debug!(%reason, "first subscribe rejected: closing socket");
                let _ = close_tx
                    .send(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::INVALID,
                        reason: reason.into(),
                    })
                    .await;
                // Hold the socket open long enough for the writer's forwarded
                // Close to reach the peer before run_session drops its halves.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return;
            }
            // Device cap or (impossible here) per-socket cap: close cleanly.
            SubscribeReject::DeviceCapReached | SubscribeReject::CapExceeded => {
                let _ = close_tx
                    .send(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::NORMAL,
                        reason: "".into(),
                    })
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return;
            }
        }
    }

    // Reader: decode each inbound frame ONCE, then route:
    //   Subscribe → register_subscribe (register another table on the shared
    //     sink — multi-table-per-handle, D1/ADR-0022).
    //   Ack/Write → handle_decoded_message (ack cursor / allowlist + write-back).
    // A rejected mid-session subscribe (per-socket cap, where_sql, or global
    // device cap) is NON-fatal: warn and keep serving existing subscriptions.
    // The reader cannot cleanly force-close the writer half after split, and
    // reject-and-continue is already bounded — `register_subscribe` returns
    // before `connect` on cap-exceed, so no session registers past
    // MAX_TABLES_PER_SOCKET (≤32 × device_cap worst case). Architecture advisor
    // (HIGH, 2026-07-15) recommended close-on-cap; this deviates because the
    // no-leak property makes close's teardown wiring unjustified — ADR-0022.
    let write_back = Arc::clone(&state.write_back);
    let write_tables = Arc::clone(&state.write_tables);
    let subs_reader = Arc::clone(&subs);
    let manager_reader = Arc::clone(&manager);
    // ADR-0025 slice 4b: read slot_epoch fresh per mid-session subscribe (it
    // bumps on slot recreate) + the op-log reader for the replay branch.
    let metrics_reader = Arc::clone(&state.metrics);
    let oplog_reader = state.oplog_reader.clone();
    // ADR-0031 D3: read fresh (not a connection-start snapshot) so a
    // mid-session subscribe issued after a live rules reload is decided
    // against the CURRENT ruleset, not a stale one — otherwise a table just
    // denied by the reload could still be granted to a newly-subscribed
    // table on an existing socket. Cold path (bounded by MAX_TABLES_PER_SOCKET
    // Subscribe frames per socket, ever), so the extra read costs nothing
    // that matters.
    let rules_for_reader = Arc::clone(&state.rules);
    let read_loop = tokio::spawn(async move {
        while let Some(Ok(msg)) = reader.next().await {
            let data: Option<Vec<u8>> = match msg {
                Message::Text(t) => Some(t.into_bytes()),
                Message::Binary(b) => Some(b),
                Message::Close(_) => break,
                _ => None, // ping/pong
            };
            let Some(data) = data else { continue };
            match decode_client_message(&data) {
                Some(ClientMessage::Subscribe {
                    table,
                    filters,
                    where_sql,
                    resume_lsn,
                    epoch,
                    rules_checksum,
                }) => {
                    let req = SubscribeRequest {
                        table,
                        filters,
                        where_sql,
                        resume_lsn,
                        client_epoch: epoch,
                        client_rules_checksum: rules_checksum,
                    };
                    let current_ruleset = rules_for_reader.read().await.clone();
                    if let Err(e) = register_subscribe(
                        &req,
                        &subs_reader,
                        &manager_reader,
                        snapshotter.as_ref(),
                        metrics_reader
                            .slot_epoch
                            .load(std::sync::atomic::Ordering::Relaxed),
                        oplog_reader.as_ref(),
                        &ack_sink,
                        &write_principal,
                        tenant_column_for_writes.as_deref(),
                        &current_ruleset,
                    )
                    .await
                    {
                        warn!(reject = ?e, table = %req.table, "mid-session subscribe rejected; socket continues");
                    }
                }
                Some(ClientMessage::SubscribeStream { id, stream, params }) => {
                    // P5: lazy mid-session stream add. Fresh ruleset read per
                    // frame (same ADR-0031 D3 discipline as Subscribe above —
                    // a stream added after a live rules reload is decided
                    // against the CURRENT ruleset).
                    let current_ruleset = rules_for_reader.read().await.clone();
                    if let Err(reason) = register_stream(
                        &id,
                        &stream,
                        &params,
                        &subs_reader,
                        &manager_reader,
                        snapshotter.as_ref(),
                        &ack_sink,
                        &write_principal,
                        tenant_column_for_writes.as_deref(),
                        &current_ruleset,
                    )
                    .await
                    {
                        // Non-fatal reject (design §1): stream_error frame,
                        // socket stays up. try_send is best-effort — a full
                        // channel means the client is gone or backlogged, and
                        // the writer loop ends on the next failed send anyway.
                        let _ = server_frames_tx.try_send(encode_stream_error(&id, &reason));
                        debug!(stream = %stream, id = %id, %reason, "subscribe_stream rejected");
                    }
                }
                Some(ClientMessage::UnsubscribeStream { id }) => {
                    unregister_stream(&id, &subs_reader, &manager_reader).await;
                }
                Some(other) => {
                    handle_decoded_message(
                        other,
                        &ack_sink,
                        &write_back,
                        &write_tables,
                        &write_principal,
                        tenant_column_for_writes.as_deref(),
                        &server_frames_tx,
                    )
                    .await;
                }
                None => warn!("dropping malformed client message"),
            }
        }
    });

    // Keep the socket alive until the writer ends, then disconnect ALL sessions
    // registered on the shared sink (one per subscribed table) + close it.
    closed.notified().await;
    sink_concrete.close();
    let ids: Vec<SessionId> = {
        let mut s = subs.lock().await;
        let mut all = std::mem::take(&mut s.ids);
        // P5: stream sessions disconnect with the socket too.
        all.extend(
            std::mem::take(&mut s.streams)
                .into_values()
                .map(|sub| sub.session),
        );
        all
    };
    for id in ids {
        manager.disconnect(id).await;
    }
    let _ = write_loop.await;
    // The reader may still be blocked on recv; abort it so the task reaps.
    read_loop.abort();
    // ADR-0029 §Decision-4: cancel any pending token-expiry deadline so a
    // short-lived session doesn't leave a sleeping timer task behind.
    if let Some(task) = exp_task {
        task.abort();
    }
}

/// The parsed first-frame subscribe request (internal shape; the wire type is
/// `ClientMessage::Subscribe`).
pub(super) struct SubscribeRequest {
    pub(super) table: String,
    pub(super) filters: Vec<crate::wire::FilterClause>,
    /// Optional safe-SQL-subset expression — compiled in `build_predicate` and
    /// ANDed in BEFORE tenant enforcement (so it can never widen scope).
    pub(super) where_sql: Option<String>,
    pub(super) resume_lsn: Option<u64>,
    /// The client's last-seen server slot epoch (ADR-0025 slice 4b). `None` on
    /// old clients → the gate treats it as a mismatch → snapshot (safe default).
    pub(super) client_epoch: Option<u64>,
    /// The client's last-synced rules checksum (ADR-0031, D2). `Some` marks a
    /// D2-or-newer client: the gate compares epoch and checksum independently.
    /// `None` marks a pre-D2 client: the gate falls back to the composed
    /// epoch (see `register_subscribe`).
    pub(super) client_rules_checksum: Option<u64>,
}

/// Await the first frame, require it to be a `ClientMessage::Subscribe`, and
/// return its fields. Returns `None` if the client closes or sends a
/// non-subscribe first frame.
///
/// A leading `Write` (ADR-0013) or `Ack` is out of order — the session must
/// subscribe first so its predicate is registered before any event (or write
/// result) flows. Same discipline as an early ACK: the socket is closed
/// (caller drops it). A `ping`/`pong` is skipped, keeping the handshake alive.
async fn read_subscribe<S, E>(socket: &mut S) -> Option<SubscribeRequest>
where
    S: futures_util::Stream<Item = Result<Message, E>> + Unpin,
{
    while let Some(Ok(msg)) = socket.next().await {
        // Collect into owned bytes so the borrow outlives the match arms.
        let data: Vec<u8> = match msg {
            Message::Text(t) => t.into_bytes(),
            Message::Binary(b) => b,
            Message::Close(_) => return None,
            _ => continue, // ping/pong — keep waiting for the subscribe
        };
        return match decode_client_message(&data)? {
            ClientMessage::Subscribe {
                table,
                filters,
                where_sql,
                resume_lsn,
                epoch,
                rules_checksum,
            } => Some(SubscribeRequest {
                table,
                filters,
                where_sql,
                resume_lsn,
                client_epoch: epoch,
                client_rules_checksum: rules_checksum,
            }),
            // An ACK, a Write, or a stream subscribe/unsubscribe (P5 §1 —
            // streams are lazy mid-session adds riding the socket's one
            // global checkpoint, so the base `Subscribe` must establish the
            // session first) before subscribing is out of order — reject by
            // closing the socket (same discipline as early ACK). The caller
            // returns from run_session, dropping the connection.
            ClientMessage::Ack { .. }
            | ClientMessage::Write { .. }
            | ClientMessage::SubscribeStream { .. }
            | ClientMessage::UnsubscribeStream { .. } => None,
        };
    }
    None
}

// The frame-transport seam (ADR-0041 D6): `run_session` + `read_subscribe`
// are generic over a `Stream + Sink` of axum `Message`s. The axum handler
// passes its upgraded `WebSocket`; the iroh accept loop (`crate::iroh_sync`)
// passes its tungstenite-over-QUIC adapter. A future third transport (e.g.
// WebTransport) adds an adapter, not a session-core change.
