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

use std::collections::BTreeMap;

use bytes::Bytes;
use futures_util::StreamExt;
use tokio_postgres::NoTls;

use nostos_domain::{Lsn, ReplicationEvent, RowOp};

use super::pg::{json_escape_into, RelationMeta};

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

    // 3. Resolve publication tables + per-table column metadata, mirroring
    //    pg.rs::bootstrap_relations_from_catalog so the JSON payload column
    //    order matches the streaming path exactly. PK detection: a column is a
    //    PK iff its attnum is a member of the primary-key index's indkey.
    let relations = catalog_relations(&sql, publication).await?;

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

/// Read publication table column metadata (oid, qualified name, column names in
/// publication order, PK indices). Same shape as `pg.rs`'s `RelationMeta`, built
/// with the same catalog query so snapshot rows and streamed rows decode
/// identically.
async fn catalog_relations(
    sql: &tokio_postgres::Client,
    publication: &str,
) -> Result<BTreeMap<i32, RelationMeta>, SnapshotError> {
    let rows = sql
        .query(
            "SELECT c.oid::int, n.nspname, c.relname, a.attname, \
                    (a.attnum = ANY (coalesce(i.indkey::int2[], ARRAY[]::int2[]))) AS is_pk \
             FROM pg_publication_tables pt \
             JOIN pg_class c ON c.relname = pt.tablename \
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = pt.schemaname \
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
             LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary \
             WHERE pt.pubname = $1 \
             ORDER BY c.oid, a.attnum",
            &[&publication],
        )
        .await
        .map_err(|e| SnapshotError::Catalog(e.to_string()))?;

    let mut by_oid: BTreeMap<i32, (String, Vec<(String, bool)>)> = BTreeMap::new();
    for row in rows {
        let oid: i32 = row.get(0);
        let nsp: String = row.get(1);
        let rel: String = row.get(2);
        let attname: String = row.get(3);
        let is_pk: bool = row.get(4);
        let qualified = if nsp == "public" || nsp.is_empty() {
            rel
        } else {
            format!("{nsp}.{rel}")
        };
        let entry = by_oid.entry(oid).or_insert_with(|| (qualified, Vec::new()));
        entry.1.push((attname, is_pk));
    }

    let mut out = BTreeMap::new();
    for (oid, (qualified, cols)) in by_oid {
        let columns: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        let mut pk_indices: Vec<usize> = cols
            .iter()
            .enumerate()
            .filter(|(_, (_, is_pk))| *is_pk)
            .map(|(i, _)| i)
            .collect();
        if pk_indices.is_empty() {
            // Defensive default matching pg.rs: if no PK is flagged, use col 0.
            pk_indices = vec![0];
        }
        out.insert(
            oid,
            RelationMeta {
                qualified_name: qualified,
                pk_indices,
                columns,
            },
        );
    }
    Ok(out)
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
        .map(|c| format!("t.{}", quote_ident(c)))
        .collect();
    let select_list = cols.join(", ");
    let copy_stmt = format!("COPY (SELECT {select_list} FROM {table_ident} t) TO STDOUT");

    // Empty-string values that coincide with COPY's text format: COPY renders
    // NULL as `\N` and an actual empty string as an empty field; it also
    // backslash-escapes `\` and `\t` within values. The streaming path renders
    // a NULL column as an empty string in the payload, so we map `\N` → "".
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
    let fields: Vec<String> = line.split('\t').map(unescape_copy_field).collect();
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

/// Unescape a single COPY text-format field. `\N` → a sentinel NULL marker
/// (returned as empty string to match the streaming path's null handling);
/// other escapes decoded per the COPY spec.
fn unescape_copy_field(field: &str) -> String {
    if field == "\\N" {
        // NULL: streaming path renders TupleDataColumn::PGNull as "" (the
        // `if let Some(Value)` falls through, leaving an empty string).
        return String::new();
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
    out
}

/// Build the JSON payload, byte-identical to `pg.rs::tuple_to_json_payload`:
/// `{"col":"val","col2":"val2",...}`, every value a JSON string. Missing/NULL
/// fields render as the empty string (matching the streaming path).
fn build_json_payload(meta: &RelationMeta, fields: &[String]) -> Vec<u8> {
    let mut out = String::from('{');
    for (i, col) in meta.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(&mut out, col);
        out.push_str("\":\"");
        if let Some(v) = fields.get(i) {
            json_escape_into(&mut out, v);
        }
        out.push('"');
    }
    out.push('}');
    out.into_bytes()
}

/// Build the PK string the way the streaming path's `pk_string` does: join the
/// PK column values with `,` (single column for our fixture schema).
fn build_pk_string(meta: &RelationMeta, fields: &[String]) -> String {
    let parts: Vec<String> = meta
        .pk_indices
        .iter()
        .filter_map(|&i| fields.get(i).cloned())
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
