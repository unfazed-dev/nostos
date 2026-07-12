//! Initial table snapshot: COPY every publication table under the slot's
//! exported snapshot so the stream starts complete (roadmap Phase 1).
//!
//! ## The gap this closes
//!
//! Without a snapshot, a client subscribing to a populated table receives
//! nothing until the first mutation — there is no "first sync" of existing
//! rows. This module reads the table contents at the slot's consistent point
//! and emits them as ready-to-deliver `Insert` events at that LSN, so a fresh
//! client gets the full current state, then seamlessly continues with live
//! streamed changes.
//!
//! ## How the snapshot is obtained (the design decision)
//!
//! `pgwire-replication` 0.3.2's [`ReplicationClient`] only issues
//! `START_REPLICATION SLOT … LOGICAL` (`client/worker.rs:186`); it does **not**
//! send `CREATE_REPLICATION_SLOT`, and the crate exposes no path to the
//! walsender-protocol `CREATE_REPLICATION_SLOT … (SNAPSHOT 'export')` variant
//! that would hand back a `snapshot_name` alongside `consistent_point`. (See
//! docs.rs/pgwire-replication/0.3.2 — there is no slot-creation API; the
//! example flow pre-creates the slot out-of-band.)
//!
//! We therefore export the snapshot the **SQL** way, which is equivalent: in a
//! single `REPEATABLE READ` transaction we call `pg_export_snapshot()` (yields
//! the importable snapshot identifier) **and**
//! `pg_create_logical_replication_slot(slot, plugin)` (yields the
//! `consistent_point` LSN). Both operations materialize against the *same*
//! snapshot, so the exported snapshot's view of the database is exactly the
//! state the slot will start streaming from. A separate connection then
//! `BEGIN; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION
//! SNAPSHOT '<id>'; COPY …` to read every publication table under that frozen
//! view. Rows committed before the consistent point are visible in the
//! snapshot; rows committed after are not — and they arrive via the live
//! stream instead. That is the exactly-once boundary (see the
//! `concurrent_writes_during_snapshot_appear_exactly_once` e2e test).
//!
//! `pg.rs::ensure_slot_and_publication` performs the slot creation + snapshot
//! export and threads `(snapshot_name, consistent_point)` back here. On a
//! restart with an EXISTING slot there is no fresh snapshot to export, so this
//! function is not called at all — no snapshot is replayed.

use bytes::Bytes;
use futures_util::StreamExt;
use tokio_postgres::NoTls;

use nostos_domain::{Lsn, ReplicationEvent, RowOp};

use super::pg::{catalog_relations, json_escape_into, RelationMeta};
use super::typed::append_typed_value;

/// Rows from all tables in `publication`, read under `snapshot_name`, encoded
/// with the same JSON payload shape the streaming path uses
/// ([`pg::tuple_to_json_payload`]). Returned as ready-to-emit `Insert` events
/// stamped at `consistent_point`.
///
/// `snapshot_name` is the identifier returned by `pg_export_snapshot()`, and
/// `consistent_point` is the LSN returned by `pg_create_logical_replication_slot()`
/// for the same slot (both captured in one REPEATABLE READ txn — see the module
/// docs for why that makes the snapshot's view == the stream start point).
///
/// # Errors
/// Connection, snapshot-import, catalog, or COPY errors bubble up as a
/// [`SnapshotError`]. The caller logs and reconnects; it does NOT surface to
/// the fan-out loop.
//
// ponytail: whole-snapshot buffered in memory; stream per-table batches through
// a channel when a real dataset exceeds RAM.
pub(crate) async fn snapshot_events(
    pg_url: &str,
    publication: &str,
    snapshot_name: &str,
    consistent_point: Lsn,
) -> Result<Vec<ReplicationEvent>, SnapshotError> {
    // 1. A dedicated connection to run the snapshot under. We keep this
    //    connection for the lifetime of the COPYs and the catalog read.
    let (sql, conn) = tokio_postgres::connect(pg_url, NoTls)
        .await
        .map_err(|e| SnapshotError::Connect(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // 2. Enter a REPEATABLE READ txn and import the exported snapshot. The
    //    snapshot identifier is short-lived: it must be imported before the
    //    exporting txn ends, so pg.rs holds that txn open until we return.
    //    (Postgres: "the snapshot ... is only accessible until the end of the
    //    transaction that exported it" — SET TRANSACTION SNAPSHOT docs.)
    sql.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .map_err(|e| SnapshotError::Snapshot(e.to_string()))?;
    // The snapshot identifier cannot be parameterized (SET TRANSACTION SNAPSHOT
    // takes a string literal). Sanitize: pg_export_snapshot returns names of
    // the form `0000000X-0000000Y`; we reject anything outside that charset to
    // avoid a SQL-injection footgun even though the value is self-produced.
    validate_snapshot_name(snapshot_name)?;
    let import = format!("SET TRANSACTION SNAPSHOT '{snapshot_name}';");
    sql.batch_execute(&import)
        .await
        .map_err(|e| SnapshotError::Snapshot(e.to_string()))?;

    // 3. Resolve publication tables + per-table column metadata via the SAME
    //    catalog query `pg.rs::bootstrap_relations_from_catalog` uses (one
    //    query, one grouping — see `pg::catalog_relations`'s docs) so the
    //    JSON payload column order AND type-OID mapping match the streaming
    //    path exactly.
    let relations = catalog_relations(&sql, publication)
        .await
        .map_err(|e| SnapshotError::Catalog(e.to_string()))?;

    // 4. Per table: COPY each column AS text, one row per line, tab-separated.
    //    We build the JSON payload with the SAME escape the streaming path uses
    //    so downstream consumers can't tell a snapshot row from a streamed one.
    // ponytail: whole-snapshot buffered in memory; stream per-table batches through
    // a channel when a real dataset exceeds RAM.
    let mut events = Vec::new();
    for meta in relations.values() {
        copy_table_rows(&sql, meta, consistent_point, &mut events).await?;
    }

    // 5. Commit (releases the imported snapshot; harmless if the exporting txn
    //    is still open elsewhere — the snapshot id simply becomes stale).
    let _ = sql.batch_execute("COMMIT;").await;
    Ok(events)
}

/// Validate the snapshot name is a Postgres snapshot identifier (hex-with-dashes)
/// before interpolating it into a `SET TRANSACTION SNAPSHOT` literal. The value
/// is produced by `pg_export_snapshot()` (never client input), but defense in
/// depth: a bogus value here would otherwise be SQL injection.
fn validate_snapshot_name(name: &str) -> Result<(), SnapshotError> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(SnapshotError::Snapshot(format!(
            "invalid snapshot identifier: {name:?}"
        )))
    }
}

/// Quote a SQL identifier the way `quote_ident` does: wrap in `"`, double any
/// embedded `"`. Names come from `pg_publication_tables` / the catalog (never
/// client input), but quote anyway — defense in depth.
fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// COPY one table's rows (each column cast to `text`) and append an `Insert`
/// event per row, stamped at `consistent_point`. The payload is built with the
/// streaming path's `tuple_to_json_payload` so a snapshot row is byte-for-byte
/// indistinguishable from a streamed insert.
async fn copy_table_rows(
    sql: &tokio_postgres::Client,
    meta: &RelationMeta,
    consistent_point: Lsn,
    out: &mut Vec<ReplicationEvent>,
) -> Result<(), SnapshotError> {
    // Build "SELECT c0::text, c1::text, ... FROM <schema>.<table>". Column
    // names come from the catalog (trusted-ish), but quote them anyway.
    let table_ident = match meta.qualified_name.rsplit_once('.') {
        Some((schema, table)) => format!("{}.{}", quote_ident(schema), quote_ident(table)),
        None => format!("public.{}", quote_ident(&meta.qualified_name)),
    };
    let cols: Vec<String> = meta
        .columns
        .iter()
        .map(|(c, _)| format!("t.{}", quote_ident(c)))
        .collect();
    let select_list = cols.join(", ");
    let copy_stmt = format!("COPY (SELECT {select_list} FROM {table_ident} t) TO STDOUT");

    // COPY renders SQL NULL as the literal `\N` and an actual empty string as
    // an empty field; it also backslash-escapes `\` and `\t` within values.
    // `unescape_copy_field` returns `None` for `\N` so NULL survives as a
    // distinct value all the way to `typed::append_typed_value`, which is
    // what lets it render as JSON `null` (not a fabricated `""`/`false`/`0`)
    // — matching the streaming path's `TupleDataColumn::PGNull` handling.
    let stream = sql
        .copy_out(&copy_stmt)
        .await
        .map_err(|e| SnapshotError::Copy(meta.qualified_name.clone(), e.to_string()))?;

    tokio::pin!(stream);
    // COPY data may arrive in multi-line chunks; buffer across chunks and split
    // on newlines. The final chunk ends with a trailing newline.
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| SnapshotError::Copy(meta.qualified_name.clone(), e.to_string()))?;
        buf.push_str(
            std::str::from_utf8(&chunk)
                .map_err(|e| SnapshotError::Copy(meta.qualified_name.clone(), e.to_string()))?,
        );
        while let Some((line, rest)) = buf.split_once('\n') {
            if !line.is_empty() {
                out.push(row_line_to_event(meta, line, consistent_point));
            }
            buf = rest.to_string();
        }
    }
    // Trailing line without a newline (rare; COPY normally ends with \n).
    let trailing = buf.trim_end_matches('\n');
    if !trailing.is_empty() {
        out.push(row_line_to_event(meta, trailing, consistent_point));
    }
    Ok(())
}

/// Parse one COPY text line (tab-separated, `\N`=NULL, `\\`/`\t` escaped) into
/// an `Insert` event whose payload matches `tuple_to_json_payload`.
fn row_line_to_event(meta: &RelationMeta, line: &str, lsn: Lsn) -> ReplicationEvent {
    // Un-escape COPY text format: split on raw tabs first, then unescape each
    // field. COPY escapes: `\N` (whole field) = NULL, `\\` → `\`, `\t` → tab,
    // `\b` → backspace, `\f` → formfeed, `\r`, `\n`. We only need `\\`, `\t`,
    // `\n`, `\r` for JSON; the rest pass through as-is (rare in these columns).
    let fields: Vec<Option<String>> = line.split('\t').map(unescape_copy_field).collect();
    let payload = build_json_payload(meta, &fields);
    let pk = build_pk_string(meta, &fields);
    ReplicationEvent::new(
        lsn,
        RowOp::Insert {
            table: meta.qualified_name.clone(),
            pk,
            payload: Bytes::from(payload),
        },
    )
}

/// Unescape a single COPY text-format field. `\N` → `None` (SQL NULL — kept
/// distinct from an actual empty string, and distinct through to
/// `typed::append_typed_value`, which is what lets NULL render as JSON `null`
/// rather than a fabricated `""`/`false`/`0`); other escapes decoded per the
/// COPY spec.
fn unescape_copy_field(field: &str) -> Option<String> {
    if field == "\\N" {
        return None;
    }
    let mut out = String::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => out.push('\\'),
                b't' => out.push('\t'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b'b' => out.push('\u{0008}'),
                b'f' => out.push('\u{000C}'),
                b'v' => out.push('\u{000B}'),
                other => {
                    out.push('\\');
                    out.push(other as char);
                }
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Some(out)
}

/// Build the JSON payload, byte-identical to `pg.rs::tuple_to_json_payload`:
/// every column goes through the SAME `typed::append_typed_value`, keyed by
/// the column's type OID — a `None` field (SQL NULL) renders as `null`; a
/// `Some(text)` field is mapped per its OID exactly like the streaming path.
fn build_json_payload(meta: &RelationMeta, fields: &[Option<String>]) -> Vec<u8> {
    let mut out = String::from('{');
    for (i, (col, type_oid)) in meta.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(&mut out, col);
        out.push_str("\":");
        let cell = fields.get(i).and_then(Option::as_deref);
        append_typed_value(&mut out, *type_oid, cell);
    }
    out.push('}');
    out.into_bytes()
}

/// Build the PK string the way the streaming path's `pk_string` does: join the
/// PK column values with `,` (single column for our fixture schema). A NULL
/// PK renders as the literal `"null"`, matching `pg.rs::pk_string`'s
/// `TupleDataColumn::PGNull` arm (PKs are practically always `NOT NULL`, but
/// this keeps the two paths's edge-case handling identical rather than
/// diverging silently).
fn build_pk_string(meta: &RelationMeta, fields: &[Option<String>]) -> String {
    let parts: Vec<String> = meta
        .pk_indices
        .iter()
        .filter_map(|&i| fields.get(i))
        .map(|v| v.clone().unwrap_or_else(|| "null".to_string()))
        .collect();
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(",")
    }
}

/// Errors from the initial snapshot. Kept flat; the caller (pg.rs) logs and
/// reconnects rather than surfacing to the fan-out loop — matching
/// `PgReplicatorError`'s discipline.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot connect error: {0}")]
    Connect(String),
    #[error("snapshot import/SET TRANSACTION SNAPSHOT error: {0}")]
    Snapshot(String),
    #[error("snapshot catalog read error: {0}")]
    Catalog(String),
    #[error("snapshot COPY error on {0}: {1}")]
    Copy(String, String),
}
