//! Snapshot-on-subscribe adapter (ADR-0014): read a table's CURRENT rows and
//! hand them to the transport as `Insert` events so a freshly-subscribing
//! client receives pre-existing rows BEFORE live fan-out (PowerSync parity).
//!
//! ## Why this exists
//!
//! Without this, nostos-server is a pure pass-through: a client that connects to
//! an already-populated table receives nothing until the first mutation. The
//! startup snapshot (`replicator::snapshot`) is drained once at boot into the
//! fan-out loop, so it only reaches clients that were connected at startup — a
//! client connecting later misses every pre-existing row. This adapter runs
//! `SELECT * FROM <table>` on subscribe and returns the rows as `Insert`
//! events; the transport delivers them to that one session before live events.
//!
//! ## Trust boundary (security-critical)
//!
//! The table name is CLIENT-CONTROLLED (it comes from the subscribe frame), so
//! it is the one string that reaches SQL. The same three-defense discipline
//! `write_back.rs` uses applies, reduced for a read-only path:
//!
//! 1. **Identifier regex FIRST.** The table is validated against
//!    `^[a-z_][a-z0-9_]*$` before any SQL is built. A name that isn't a bare
//!    lowercase identifier is rejected — it can never reach the query builder.
//! 2. **Quoted identifier.** The validated name is emitted wrapped in `"…"`
//!    (with embedded `"` doubled), so even a hypothetical regex bypass would be
//!    contained by the quote.
//! 3. **No value interpolation.** Snapshot reads values; it never binds
//!    client-supplied values. There is no `$1…$n` surface here, so the
//!    value-injection defense from the write path is moot. The SELECT is a
//!    bare `SELECT "<cols>"::text, … FROM "<table>"` with zero parameters.
//!
//! Column NAMES are NOT client-controlled (they come from the prepared
//! statement's metadata via `pg_attribute`, not the subscribe frame), but they
//! are quoted anyway — defense in depth, same as `replicator::snapshot`.
//!
//! ## Payload fidelity (ADR-0019)
//!
//! Each row's payload is built with the SAME `append_typed_value` OID-keyed
//! mapping the streaming path (`pg::tuple_to_json_payload`) and the startup
//! snapshot (`snapshot::build_json_payload`) use, so a snapshot row is
//! byte-for-byte indistinguishable from a streamed insert. The column OIDs come
//! from the prepared statement's metadata; each cell is fetched as Postgres
//! TEXT (via `::text` cast) — the exact wire form `append_typed_value` expects.
//!
//! `unsafe` is forbidden crate-wide. This module performs no I/O of its own
//! beyond a single read-only `SELECT` per subscribe.

#![cfg(feature = "pg")]

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use tokio_postgres::NoTls;

use nostos_application::ports::{SnapshotError, SnapshotSource};
use nostos_domain::{Lsn, ReplicationEvent, RowOp};

/// Reuse the streaming path's OID-keyed JSON mapping (ADR-0019) so snapshot
/// rows render byte-identically to streamed rows. Both helpers are `pub(crate)`
/// in the replicator module — reusing them (rather than re-implementing) is
/// what guarantees a snapshot row and a streamed insert of the same row
/// produce the same payload bytes.
use crate::replicator::pg::json_escape_into;
use crate::replicator::typed::append_typed_value;

/// v1 convention: the primary-key column is fixed to `id`. Mirrors
/// `PgWriteBack`'s `PK_COLUMN`. ponytail: discover the pk from `pg_constraint`
/// when a design partner needs composite / renamed primary keys.
const PK_COLUMN: &str = "id";

/// A `SnapshotSource` that reads current rows from the source Postgres via a
/// pool-of-one `tokio_postgres::Client` (mirrors `PgWriteBack`'s lazy-connect
/// discipline). Constructed by the composition root under
/// `NOSTOS_REPLICATOR=pg`; injected into the transport via `.with_snapshotter`.
///
/// ponytail: per-subscribe `SELECT *` cost + whole-table buffered in memory.
/// Ceiling: one PG round-trip + one full table scan per subscribe, which is
/// fine for the OSS self-host / single-tenant dev target (small tables, low
/// connect rate). Upgrade path: an in-memory materialized view maintained by
/// the live fan-out events (each upsert/delete updates the view; a subscribe
/// reads the view with no PG round-trip). Build the view when a real load
/// number says the per-subscribe SELECT is the bottleneck — not before.
pub struct PgSnapshotter {
    pg_url: String,
    /// Pool-of-one. `Mutex` (not `OnceCell`) so a dead connection can be
    /// replaced transparently on the next subscribe — same discipline as
    /// `PgWriteBack`.
    client: Arc<Mutex<Option<tokio_postgres::Client>>>,
}

impl PgSnapshotter {
    /// Construct with a libpq-style URL. Does NOT connect — the first
    /// subscribe opens the connection lazily (and reopens it transparently if
    /// it ever dies).
    #[must_use]
    pub fn new(pg_url: &str) -> Self {
        Self {
            pg_url: pg_url.to_string(),
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// Obtain a connected client, opening the connection lazily if none is
    /// cached (identical to `PgWriteBack::client`).
    async fn client(&self) -> Result<tokio_postgres::Client, SnapshotError> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.take() {
            return Ok(c);
        }
        let (client, conn) = tokio_postgres::connect(&self.pg_url, NoTls)
            .await
            .map_err(|e| SnapshotError::Backend(format!("connect: {e}")))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(client)
    }

    /// Return a client to the pool after a successful read.
    async fn return_client(&self, client: tokio_postgres::Client) {
        let mut guard = self.client.lock().await;
        *guard = Some(client);
    }

    /// Drop the client slot after an error that may have killed the
    /// connection. The next subscribe reopens.
    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        *guard = None;
    }
}

#[async_trait]
impl SnapshotSource for PgSnapshotter {
    async fn snapshot(
        &self,
        table: &str,
        base_lsn: Lsn,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        // 1. TRUST BOUNDARY: validate the table identifier BEFORE any SQL is
        //    built. The name is client-controlled (subscribe frame); the regex
        //    rejects anything that isn't a bare lowercase identifier, so it can
        //    never break out of the identifier-quote the builder adds.
        if let Err(bad) = validate_ident(table) {
            return Err(SnapshotError::InvalidTable(bad));
        }
        let quoted_table = quote_ident(table);

        let client = self.client().await?;

        // 2. Prepare `SELECT * FROM <table>` to read the column names + type
        //    OIDs from the statement's metadata (zero rows needed — a prepared
        //    statement carries its column descriptors without executing). The
        //    OIDs drive the OID-keyed JSON mapping so the snapshot payload is
        //    byte-identical to the streaming path's (ADR-0019).
        let prep = match client
            .prepare(&format!("SELECT * FROM {quoted_table}"))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.drop_client().await;
                return Err(SnapshotError::Backend(format!("prepare: {e}")));
            }
        };
        // (name, type OID). OIDs are u32 on the wire; append_typed_value takes
        // i32 (Postgres OIDs are always < 2^31). A convert failure yields -1,
        // which append_typed_value treats as "unrecognized" → string
        // passthrough — never a panic, never a dropped row.
        let cols: Vec<(&str, i32)> = prep
            .columns()
            .iter()
            .map(|c| {
                let oid = i32::try_from(c.type_().oid()).unwrap_or(-1);
                (c.name(), oid)
            })
            .collect();

        // PK column index (v1: the column named "id"). ponytail: read from
        // pg_constraint for composite/renamed PKs (mirrors PgWriteBack).
        let pk_index = cols.iter().position(|(n, _)| *n == PK_COLUMN);

        // 3. Build the text-cast SELECT. Every column is cast to `::text` so
        //    each cell comes back as the Postgres TEXT wire form — the exact
        //    shape `append_typed_value` consumes (same as COPY's text output,
        //    which is what `replicator::snapshot` parses). Column names came
        //    from the catalog (trusted), but quote them anyway — defense in
        //    depth, same as the startup snapshot's `quote_ident`.
        let select_list = cols
            .iter()
            .map(|(n, _)| format!("{}::text", quote_ident(n)))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {select_list} FROM {quoted_table}");
        let rows = match client.query(&sql, &[]).await {
            Ok(r) => r,
            Err(e) => {
                self.drop_client().await;
                return Err(SnapshotError::Backend(format!("query: {e}")));
            }
        };

        // 4. Build one Insert event per row. Each gets a UNIQUE LSN strictly
        //    above `base_lsn` so the per-session sink's LSN gate
        //    (`TokioEventSink::deliver` drops `lsn <= acked && acked != 0`,
        //    and the dedup ring drops exact-LSN duplicates) does NOT swallow
        //    snapshot rows. For a fresh client base_lsn = 0, so events are
        //    stamped 1, 2, 3, … — far below any real WAL LSN, so live fan-out
        //    events (which carry real Postgres WAL positions, typically in the
        //    millions+) always sort above the synthetic range and are never
        //    mis-dropped.
        //
        //    ponytail: LSNs share the single u64 space with real WAL LSNs. For
        //    a RESUMING client (base_lsn = a real WAL position N), synthetic
        //    LSNs N+1..N+M could in principle collide with a real WAL event in
        //    that narrow window, dropping one live event. This is not
        //    attacker-reachable and the primary target case (fresh subscribe,
        //    base_lsn = 0) is unaffected. Upgrade path: a per-session synthetic
        //    LSN band (e.g. reserve a high bit), or skip-snapshot-on-resume
        //    (a resuming client already has the table state from its prior
        //    session). Revisit when a resume-mode design partner reports it.
        let mut events = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let payload = build_payload(&cols, row);
            // PK: the "id" column's text value if present, else the row index
            // (a stable fallback so the event is still well-formed; the client
            // upserts by pk and a missing-id table is out of v1 scope).
            let pk = pk_index
                .and_then(|idx| row.try_get::<usize, Option<String>>(idx).ok().flatten())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| i.to_string());
            let lsn = Lsn::new(base_lsn.raw().saturating_add(1).saturating_add(i as u64));
            events.push(ReplicationEvent::new(
                lsn,
                RowOp::Insert {
                    table: table.to_string(),
                    pk,
                    payload: Bytes::from(payload),
                },
            ));
        }

        self.return_client(client).await;
        Ok(events)
    }
}

/// Build the JSON payload for one row, byte-identical to the streaming path's
/// `tuple_to_json_payload` / the startup snapshot's `build_json_payload`: every
/// column goes through the SAME `append_typed_value`, keyed by its type OID,
/// and every column name goes through the SAME `json_escape_into`. A SQL NULL
/// cell (`Option<String>::None`) renders as JSON `null` — not a fabricated
/// `""`/`false`/`0` — exactly like the streaming path.
fn build_payload(cols: &[(&str, i32)], row: &tokio_postgres::Row) -> Vec<u8> {
    let mut out = String::with_capacity(128);
    out.push('{');
    for (i, (name, oid)) in cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(&mut out, name);
        out.push_str("\":");
        // ::text cast in the SELECT makes every column a TEXT value; NULL stays
        // NULL (Option::None). try_get never panics on a type mismatch — a
        // failure here would be a Postgres protocol bug, so flatten to None
        // (renders as JSON null) rather than crashing the subscribe path.
        let cell = row.try_get::<usize, Option<String>>(i).ok().flatten();
        append_typed_value(&mut out, *oid, cell.as_deref());
    }
    out.push('}');
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Identifier defense — identical to `write_back.rs::pg`'s helpers. Duplicated
// rather than shared because the helpers are private to write_back's `pg`
// submodule and the snapshot path is a distinct port (snapshot_source.rs).
// Keeping a second copy is deliberate: the two ports must not gain a hidden
// coupling through a shared private helper module, and the copies are tiny
// (one regex + a one-line quote). If a THIRD caller appears, lift these into a
// shared `crate::ident` module at that point.
// ---------------------------------------------------------------------------

/// The strict identifier regex: a bare lowercase SQL identifier
/// (`^[a-z_][a-z0-9_]*$`). Applied to the table name before any SQL is built.
/// This is the SAME pattern `PgWriteBack` uses — the structural injection
/// defense for a client-controlled identifier.
fn ident_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^[a-z_][a-z0-9_]*$")
            .expect("identifier regex is a valid static pattern")
    })
}

/// Validate one identifier against the strict regex. Returns `Ok(())` if it
/// matches, or `Err` with the offending identifier so the caller can wrap it
/// in a [`SnapshotError::InvalidTable`].
fn validate_ident(name: &str) -> Result<(), String> {
    if ident_regex().is_match(name) {
        Ok(())
    } else {
        Err(name.to_string())
    }
}

/// Wrap a validated identifier in Postgres double-quotes (the identifier
/// quote). The caller MUST have run [`validate_ident`] first — this function
/// does not re-check, it only quotes (belt-and-braces on top of the regex).
fn quote_ident(name: &str) -> String {
    // Postgres escapes a literal `"` inside an identifier by doubling it. The
    // regex already guarantees no `"` can be present, but doubling is correct
    // regardless and costs nothing.
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ident_accepts_bare_lowercase() {
        assert!(validate_ident("tasks").is_ok());
        assert!(validate_ident("org_id").is_ok());
        assert!(validate_ident("_private").is_ok());
        assert!(validate_ident("col2").is_ok());
    }

    #[test]
    fn validate_ident_rejects_injection_attempts() {
        // The security-critical cases: anything that isn't a bare lowercase
        // identifier must be rejected before it can reach quote_ident().
        assert!(validate_ident("a; DROP TABLE x").is_err());
        assert!(validate_ident("col\"--").is_err());
        assert!(validate_ident("Title").is_err()); // uppercase
        assert!(validate_ident("col name").is_err()); // space
        assert!(validate_ident("schema.table").is_err()); // dot
        assert!(validate_ident("1col").is_err()); // leading digit
        assert!(validate_ident("").is_err()); // empty
        assert!(validate_ident("café").is_err()); // non-ascii
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("tasks"), "\"tasks\"");
        // A hypothetical escaped quote — doubled, not backslashed. The regex
        // guarantees none reaches here in production; correct regardless.
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn lsn_assignment_is_strictly_above_base_and_unique() {
        // Mirror the snapshot() LSN math to assert the gate-safety invariant
        // without a database: every event LSN must be > base and all unique.
        let base = Lsn::new(100);
        let row_count = 5_usize;
        let mut lsns: Vec<u64> = (0..row_count)
            .map(|i| base.raw().saturating_add(1).saturating_add(i as u64))
            .collect();
        assert!(lsns.iter().all(|&l| l > base.raw()));
        lsns.sort_unstable();
        lsns.dedup();
        assert_eq!(lsns.len(), row_count, "snapshot LSNs must be unique");
    }

    #[test]
    fn fresh_subscribe_base_zero_starts_at_one() {
        // The primary target case: fresh client, base_lsn = 0. First event
        // must be 1 (strictly > 0 AND non-zero so the sink's `acked != 0`
        // guard doesn't even apply, but >0 is required to stay clear of the
        // dedup ring's 0 sentinel behavior).
        let base = Lsn::new(0);
        let first = base.raw().saturating_add(1).saturating_add(0);
        assert_eq!(first, 1);
    }
}
