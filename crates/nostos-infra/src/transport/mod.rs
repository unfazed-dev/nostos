//! WebSocket transport adapter — axum route that authenticates a connection,
//! upgrades it, spawns a session, drains its bounded sink onto the wire, and
//! reads client ACKs concurrently.
//!
//! Flow on a new connection:
//! 1. **Authenticate** the bearer token (Authorization header OR `?token=` —
//!    browsers can't set headers on a WS handshake) via the `SyncAuth` port.
//!    Reject with HTTP 401 before upgrade on failure (ADR-0010). This closes
//!    the data-exfiltration hole the unauthenticated `/sync` had.
//! 2. Read the first frame as a `ClientMessage::Subscribe { table, filters,
//!    resume_lsn }`.
//! 3. **Inject the tenant filter** from the principal into the predicate (the
//!    client's own filters are intersected, never allowed to widen scope —
//!    ADR-0011). Anonymous principals get no injection.
//! 4. Allocate a `TokioEventSink` (bounded channel); seed its ack cursor from
//!    `resume_lsn` if present.
//! 5. Register the authenticated session with the `SessionManager`.
//! 6. **Split** the socket: a writer task drains the sink onto the wire; a
//!    reader task parses `ClientMessage::Ack` frames and stamps the sink's ack
//!    cursor (driving the ack-driven slot advance, ADR-0009).
//! 7. On disconnect, close the sink + unregister.

mod dispatch;
mod handshake;
mod scope;
mod session;
mod state;
mod subscribe;
#[cfg(test)]
mod test_support;

#[cfg(feature = "iroh")]
pub(crate) use handshake::bearer_token;
pub use handshake::{sync_handler, AuthQuery};
#[cfg(feature = "iroh")]
pub(crate) use session::run_session;
pub use state::SyncRouterState;

/// Default per-session bounded-buffer depth. Slow clients that fall this far
/// behind are dropped (an explicit, observable choice — never silent OOM).
const DEFAULT_SESSION_BUFFER: usize = 1024;

/// Max frames coalesced into one WebSocket message under backlog (C3
/// batched-writes). The write task drains up to this many *immediately
/// available* frames after the first; the first always arrives via an
/// `await`, so there is **zero latency tax at low rates** — batching only
/// kicks in when the channel already has a backlog (≥2 pending frames). The
/// receiver decodes both the batched array and the legacy single-object form,
/// so no wire-version bump is needed.
const MAX_BATCH_FRAMES: usize = 64;

/// Per-socket table-subscription cap (D1/ADR-0022). Bounds snapshot-on-
/// subscribe cost (each subscribe triggers a full-table SELECT in
/// `PgSnapshotter`) so one client cannot DoS the snapshotter by subscribing to
/// thousands of tables on one socket. A `Subscribe` beyond this cap is
/// rejected (non-fatal — the socket keeps serving its existing subscriptions);
/// 32 is generous for real apps (the provider dashboard uses 5) and small
/// enough that 32 × device_cap snapshots is a bounded worst case.
const MAX_TABLES_PER_SOCKET: usize = 32;

/// Largest inbound WS message the server will buffer, per connection.
///
/// axum 0.7 defaults to 64 MiB message / 16 MiB frame. Everything a client
/// sends us is small JSON — subscribe, ack, write-back mutation — and blobs go
/// out-of-band on the attachment plane (ADR-0034), so the default is three
/// orders of magnitude more headroom than the protocol needs. It is also
/// per-connection: at a 1k-client device cap the default admits a ~64 GB
/// worst-case buffer ceiling, reachable by clients that authenticate and then
/// simply send large frames.
///
/// ponytail: one flat cap for every inbound message type. If write-back ever
/// needs to carry a genuinely large batch, give that message type its own
/// higher cap rather than raising this one for the whole socket.
const MAX_WS_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Largest single inbound WS frame. Messages may span frames, so this bounds
/// per-read allocation while `MAX_WS_MESSAGE_BYTES` bounds the reassembled whole.
const MAX_WS_FRAME_BYTES: usize = 1024 * 1024;

/// Close reason a live session receives when a sync-rules reload (ADR-0031
/// D3) changes the rule decision for one of its subscribed tables. Swap
/// verification is per-table and coarse — see the `ponytail:` at the
/// `run_session` call site — so this fires on a real narrowing AND on any
/// widen the verification can't prove safe in place; either way the client's
/// reconnect (Task 11's checksum/epoch path) re-scopes it into the current
/// ruleset.
/// The wire-contract close reason for "ruleset changed under you; reconnect
/// to re-scope" — PUBLIC because clients (nostos-client) must distinguish this
/// one legitimate INVALID(1008) close from a subscribe REJECTION (same code,
/// different reason). Do not change the string: it is a cross-process
/// contract asserted by ws_contract tests and matched client-side.
pub const RULES_CHANGED_CLOSE_REASON: &str = "rules changed; reconnect to re-scope";
