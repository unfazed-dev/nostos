//! Write-back adapters — apply client-submitted writes to the source database
//! (ADR-0013 v1).
//!
//! Two implementations of [`nostos_application::ports::WriteBack`]:
//!
//! - [`PgWriteBack`] (feature `pg`) — the real adapter. Upserts/deletes a row
//!   in the source Postgres via a pool-of-one `tokio_postgres::Client`. The
//!   row change then flows back out through normal logical replication to
//!   every subscriber, including the writer (where the idempotent apply is a
//!   no-op). LWW by WAL order.
//! - [`NoWriteBack`] — the fake-mode stub. Returns
//!   [`WriteBackError::Backend`] for every call: the `FakeReplicator` has no
//!   database to write to, so v1 surfaces a clear "write-back requires pg
//!   replicator" error rather than silently dropping the write.
//!
//! ## Trust boundary (security-critical)
//!
//! The write-back path is the one place a *client-controlled* string becomes
//! part of a SQL statement. Three defenses, applied in order, make that safe:
//!
//! 1. **Allowlist FIRST.** The table must be in `NOSTOS_WRITE_TABLES`
//!    (exact match against a `HashSet`). A table not explicitly writable can
//!    never reach the SQL builder, so its name can never be interpolated. This
//!    is the gate — it runs before anything else.
//! 2. **Identifier regex.** Table + every column name validated against
//!    `^[a-z_][a-z0-9_]*$`. The check is structural: it rejects anything that
//!    isn't a bare lowercase identifier, so a column name like `a"; DROP` or
//!    `col--` cannot break out of the identifier-quote the builder adds.
//! 3. **Parameterized values.** Every value is bound as `$1…$n`. No value is
//!    ever string-interpolated into the SQL — not "just for v1," not ever.
//!    This is the second injection defense (the first being the identifier
//!    regex on the keys).
//!
//! The identifier-quote (`"col"`) the builder emits is belt-and-braces on top
//! of the regex: even if a malicious identifier somehow passed the regex, the
//! quote would contain it. Defense-in-depth — the regex is the rule, the
//! quote is the backstop.
//!
//! `unsafe` is forbidden crate-wide (`#![forbid(unsafe_code)]` in lib.rs).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use nostos_application::ports::{WriteBack, WriteBackError};
use nostos_domain::TenantScope;

/// Parse the comma-separated `NOSTOS_WRITE_TABLES` env value into a `HashSet`
/// of bare table names. Empty entries are skipped; whitespace is trimmed.
/// An empty/absent env var yields an empty set (no tables writable). Used by
/// the composition root to build [`PgWriteBack`]'s allowlist (and to seed the
/// transport's allowlist gate).
#[must_use]
pub fn parse_allowlist(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `NOSTOS_OR_SET_COLUMNS` — a comma-separated list of `table:column`
/// pairs naming the JSONB columns that hold add-wins OR-sets (ADR-0030). Each
/// pair maps a table to the single column holding its element set, so
/// [`PgWriteBack`] knows which writes to merge element-wise server-side instead
/// of clobbering. Empty/absent ⇒ no OR-set columns (the default); OR-set writes
/// to unconfigured tables are rejected client-side (`SyncClientConfig`).
/// Example: `tasks:tags,notes:labels`. Malformed entries (no `:` or an empty
/// side) are silently skipped — a loud failure belongs at the client API, not
/// at config parse time.
pub fn parse_or_set_columns(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (table, col) = pair.split_once(':')?;
            let table = table.trim();
            let col = col.trim();
            if table.is_empty() || col.is_empty() {
                return None;
            }
            Some((table.to_string(), col.to_string()))
        })
        .collect()
}

/// Parse `NOSTOS_COUNTER_COLUMNS` (same `table:col` format as
/// [`parse_or_set_columns`]) into the PN-Counter column map (ADR-0030 addendum).
/// A table maps to the JSONB column that holds its counter payload
/// `{"entries":[{"r":..,"p":..,"n":..}]}`.
pub fn parse_counter_columns(raw: &str) -> HashMap<String, String> {
    // Identical parse format — separate function for doc-clarity + the "three
    // views of one truth" config audit trail.
    parse_or_set_columns(raw)
}

// ===========================================================================
// NoWriteBack — the fake-mode stub (always available, no feature gate).
// ===========================================================================

/// A `WriteBack` that refuses every call. Injected when the server runs the
/// `FakeReplicator` (no source database exists), so a client attempting a
/// write gets a clear `write-back requires pg replicator` error instead of a
/// silent drop.
///
/// The error message is fixed so the contract test can assert it verbatim.
pub struct NoWriteBack;

impl NoWriteBack {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for NoWriteBack {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WriteBack for NoWriteBack {
    async fn upsert(
        &self,
        _table: &str,
        _pk: &str,
        _payload_json: &str,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        Err(WriteBackError::Backend(
            "write-back requires pg replicator".to_string(),
        ))
    }

    async fn delete(
        &self,
        _table: &str,
        _pk: &str,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        Err(WriteBackError::Backend(
            "write-back requires pg replicator".to_string(),
        ))
    }

    async fn patch(
        &self,
        _table: &str,
        _pk: &str,
        _payload_json: &str,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        Err(WriteBackError::Backend(
            "write-back requires pg replicator".to_string(),
        ))
    }

    async fn increment(
        &self,
        _table: &str,
        _pk: &str,
        _payload_json: &str,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        Err(WriteBackError::Backend(
            "write-back requires pg replicator".to_string(),
        ))
    }
}

// ===========================================================================
// PgWriteBack — the real adapter (feature "pg").
// ===========================================================================
#[cfg(feature = "pg")]
mod pg {
    use async_trait::async_trait;
    use nostos_application::ports::{WriteBack, WriteBackError};
    use nostos_domain::TenantScope;
    use std::collections::{HashMap, HashSet};
    use std::fmt::Write as _; // ponytail: single write!() for the $n placeholder
    use std::sync::Arc;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    /// The strict identifier regex: a bare lowercase SQL identifier
    /// (`^[a-z_][a-z0-9_]*$`). Applied to the table name AND every payload
    /// column name before any SQL is built. This is the structural injection
    /// defense — it rejects any identifier that isn't a plain lowercase name,
    /// so a client-controlled column can't break out of the identifier-quote.
    ///
    /// `OnceLock` because `Regex::new` is not `const`; the lock caches the
    /// single compilation for the process lifetime.
    fn ident_regex() -> &'static regex::Regex {
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        RE.get_or_init(|| {
            regex::Regex::new(r"^[a-z_][a-z0-9_]*$")
                .expect("identifier regex is a valid static pattern")
        })
    }

    /// Validate one identifier against the strict regex. Returns `Ok(())` if
    /// it matches, or `Err` with the offending identifier so the caller can
    /// wrap it in the right [`WriteBackError`] variant.
    fn validate_ident(name: &str) -> Result<(), String> {
        if ident_regex().is_match(name) {
            Ok(())
        } else {
            Err(name.to_string())
        }
    }

    /// Wrap a validated identifier in Postgres double-quotes (the identifier
    /// quote). The caller MUST have run [`validate_ident`] first — this
    /// function does not re-check, it only quotes. The quote is belt-and-
    /// braces on top of the regex (defense-in-depth): even a hypothetical
    /// regex bypass would be contained by the quoting.
    fn quote_ident(name: &str) -> String {
        // Postgres escapes a literal `"` inside an identifier by doubling it.
        // The regex already guarantees no `"` can be present, but doubling is
        // correct regardless and costs nothing.
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// v1 convention: the primary-key column is fixed to `"id"`.
    /// ponytail: pk column fixed to "id"; read from pg_constraint when a
    /// design partner needs composite/renamed primary keys.
    const PK_COLUMN: &str = "id";

    /// A `WriteBack` that applies writes to the source Postgres via a
    /// pool-of-one `tokio_postgres::Client`.
    ///
    /// ponytail: single connection; pool when a real load shows contention.
    /// The connection is established lazily on the first write and reused;
    /// a broken connection is re-opened transparently on the next call.
    /// The rejection text for a write that a tenant guard refused.
    ///
    /// Deliberately says nothing about *why*. The old text — "row {pk} in {table}
    /// belongs to a different tenant" — confirmed to the caller that the pk exists
    /// and is owned by someone else, turning the write path into a cross-tenant
    /// ownership oracle over guessed primary keys (audit finding 9).
    ///
    /// Residual, stated honestly: this closes the *disclosure*, not the whole
    /// oracle. A pk that does not exist still inserts successfully while a
    /// cross-tenant pk is refused, so the outcome alone still separates the two
    /// cases. Closing that would mean either letting the write through (wrong) or
    /// reporting a success that did not happen — the write path must never tell a
    /// client its write landed when it did not (ADR-0018). Rate limiting is the
    /// mitigation for enumeration; a vaguer error is not.
    const CROSS_TENANT_REJECTION: &str = "write refused by the tenant guard";

    pub struct PgWriteBack {
        pg_url: String,
        allowlist: HashSet<String>,
        /// ADR-0030 slice 3: table → JSONB column holding its OR-set element
        /// set. Writes to these tables merge element-wise (read-modify-write)
        /// instead of clobbering, so concurrent client adds converge server-side.
        or_set_columns: HashMap<String, String>,
        /// ADR-0030 addendum: table → JSONB column holding its PN-Counter
        /// payload. Writes to these tables merge per-replica elementwise max
        /// (state-based CRDT), so concurrent client increments converge
        /// server-side without a server-side read-modify-write race.
        counter_columns: HashMap<String, String>,
        /// Pool-of-one. `Mutex` (not `OnceCell`) so a dead connection can be
        /// replaced: we take the lock, probe/execute, and on a fatal error
        /// drop the inner `Client` (the next call reconnects).
        client: Arc<Mutex<Option<tokio_postgres::Client>>>,
    }

    impl PgWriteBack {
        /// Construct with a libpq-style URL and the parsed `NOSTOS_WRITE_TABLES`
        /// allowlist. Does NOT connect — the first write opens the connection
        /// lazily (and reopens it transparently if it ever dies).
        #[must_use]
        pub fn new(pg_url: &str, allowlist: HashSet<String>) -> Self {
            Self {
                pg_url: pg_url.to_string(),
                allowlist,
                or_set_columns: HashMap::new(),
                counter_columns: HashMap::new(),
                client: Arc::new(Mutex::new(None)),
            }
        }

        /// ADR-0030 slice 3: configure OR-set tables (table → the JSONB column
        /// holding the element set). Builder, mirroring the client's
        /// `with_or_set_tables`. Writes to these tables merge element-wise.
        #[must_use]
        pub fn with_or_set_columns(mut self, cols: HashMap<String, String>) -> Self {
            self.or_set_columns = cols;
            self
        }

        /// ADR-0030 addendum: configure PN-Counter tables (table → the JSONB
        /// column holding the counter payload). Builder, mirroring the client's
        /// `with_counter_tables`. Writes to these tables merge per-replica max.
        #[must_use]
        pub fn with_counter_columns(mut self, cols: HashMap<String, String>) -> Self {
            self.counter_columns = cols;
            self
        }

        /// Obtain a connected client, opening the connection lazily if none is
        /// cached. The connection's background task is spawned and forgotten
        /// (tokio-postgres drives the socket on it; dropping the `Client`
        /// closes the socket).
        async fn client(&self) -> Result<tokio_postgres::Client, WriteBackError> {
            let mut guard = self.client.lock().await;
            if let Some(c) = guard.take() {
                return Ok(c);
            }
            // Open a fresh connection (bounded — audit M8).
            crate::pg_connect::pg_connect_bounded(&self.pg_url)
                .await
                .map_err(WriteBackError::Backend)
        }

        /// Return a client to the pool (called after a successful statement).
        async fn return_client(&self, client: tokio_postgres::Client) {
            let mut guard = self.client.lock().await;
            *guard = Some(client);
        }

        /// Drop the client slot — called after an error that may have killed
        /// the connection. The next call will reopen.
        async fn drop_client(&self) {
            let mut guard = self.client.lock().await;
            *guard = None;
        }

        /// ADR-0030 slice 3: merge a flushed OR-set payload element-wise into the
        /// configured column (read-modify-write with the pool guard HELD across
        /// the whole RMW — single writer per row, audit 2026-08-17 M4: the old
        /// check-out-then-query pattern let a concurrent merge open a SECOND
        /// connection, and two same-row merges could both SELECT the same
        /// state — one element silently lost).
        ///
        /// Tenant-scoped when `tenant` is `Some` (ADR-0018 extended to the
        /// merge paths): the SELECT is scoped so a merge never READS another
        /// tenant's state into ours, the INSERT stamps the tenant column with
        /// the PRINCIPAL's value, and the ON CONFLICT carries the same guard
        /// as the clobber path — a conflict against a row owned by a different
        /// tenant affects 0 rows and surfaces as `Forbidden` instead of a
        /// silent ownership change. The tenant binds go through
        /// prepare + `coerce_params` (unlike the no-tenant path, kept
        /// byte-identical) so a uuid-shaped tenant value binds correctly
        /// against BOTH text and uuid tenant columns.
        async fn or_set_merge(
            &self,
            table: &str,
            pk: &str,
            col: &str,
            payload_json: &str,
            tenant: Option<TenantScope<'_>>,
        ) -> Result<(), WriteBackError> {
            if let Err(bad) = validate_ident(col) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad OR-set column identifier: {bad}"
                )));
            }
            if let Some(t) = tenant {
                if let Err(bad) = validate_ident(t.column) {
                    return Err(WriteBackError::InvalidPayload(format!(
                        "bad tenant column identifier: {bad}"
                    )));
                }
            }
            let quoted_table = quote_ident(table);
            let quoted_pk = quote_ident(PK_COLUMN);
            let quoted_col = quote_ident(col);
            let pk_value = SqlValue::from_pk(pk);

            // Hold the pool guard across the WHOLE read-modify-write so a
            // concurrent same-row merge serializes behind us instead of
            // opening a second connection and reading the same pre-merge
            // state. On error the slot stays None (client dropped with the
            // guard); the next call reconnects — same as drop_client.
            let mut guard = self.client.lock().await;
            let client = match guard.take() {
                Some(c) => c,
                None => crate::pg_connect::pg_connect_bounded(&self.pg_url)
                    .await
                    .map_err(WriteBackError::Backend)?,
            };
            // Read the existing element-set (NULL / absent row → empty → just the
            // incoming set). Cast jsonb → text so the bytes round-trip through
            // serde_json unchanged.
            let existing: Option<String> = match tenant {
                None => {
                    let select_sql = format!(
                        "SELECT {quoted_col}::text FROM {quoted_table} WHERE {quoted_pk} = $1"
                    );
                    let sel_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        vec![pk_value.as_tosql()];
                    match client.query_opt(&select_sql, &sel_params).await {
                        Ok(Some(row)) => row.get::<_, Option<String>>(0),
                        Ok(None) => None,
                        Err(e) => {
                            return Err(WriteBackError::Backend(e.to_string()));
                        }
                    }
                }
                Some(t) => {
                    let quoted_tcol = quote_ident(t.column);
                    let select_sql = format!(
                        "SELECT {quoted_col}::text FROM {quoted_table} \
                         WHERE {quoted_pk} = $1 AND {quoted_tcol} = $2"
                    );
                    let mut sel_values =
                        vec![SqlValue::from_pk(pk), SqlValue::from_scalar(t.value)];
                    let stmt = match client.prepare(&select_sql).await {
                        Ok(s) => s,
                        Err(e) => return Err(WriteBackError::Backend(e.to_string())),
                    };
                    if let Err(msg) = coerce_params(&stmt, &mut sel_values) {
                        return Err(WriteBackError::InvalidPayload(msg));
                    }
                    let sel_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        sel_values.iter().map(SqlValue::as_tosql).collect();
                    match client.query_opt(&stmt, &sel_params).await {
                        Ok(Some(row)) => row.get::<_, Option<String>>(0),
                        Ok(None) => None,
                        Err(e) => {
                            return Err(WriteBackError::Backend(e.to_string()));
                        }
                    }
                }
            };
            let existing_bytes = existing.as_deref().map_or(&b""[..], str::as_bytes);
            let merged = nostos_domain::merge_or_set_or_lww(existing_bytes, payload_json.as_bytes());
            // Bind merged JSON as jsonb (parse → Value → SqlValue::Json, matching
            // the clobber path's object/array binding).
            let merged_value: serde_json::Value =
                serde_json::from_slice(&merged).unwrap_or(serde_json::Value::Null);
            let col_value = json_value_to_sql(&merged_value);

            match tenant {
                None => {
                    let sql = format!(
                        "INSERT INTO {quoted_table} ({quoted_pk}, {quoted_col}) \
                         VALUES ($1, $2) \
                         ON CONFLICT ({quoted_pk}) DO UPDATE SET {quoted_col} = EXCLUDED.{quoted_col}"
                    );
                    let ins_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        vec![pk_value.as_tosql(), col_value.as_tosql()];
                    match client.execute(&sql, &ins_params).await {
                        Ok(_) => {
                            *guard = Some(client);
                            Ok(())
                        }
                        Err(e) => Err(WriteBackError::Backend(e.to_string())),
                    }
                }
                Some(t) => {
                    let quoted_tcol = quote_ident(t.column);
                    let sql = format!(
                        "INSERT INTO {quoted_table} ({quoted_pk}, {quoted_col}, {quoted_tcol}) \
                         VALUES ($1, $2, $3) \
                         ON CONFLICT ({quoted_pk}) DO UPDATE SET {quoted_col} = EXCLUDED.{quoted_col} \
                         WHERE {quoted_table}.{quoted_tcol} = EXCLUDED.{quoted_tcol}"
                    );
                    let mut ins_values = vec![
                        SqlValue::from_pk(pk),
                        col_value,
                        SqlValue::from_scalar(t.value),
                    ];
                    let stmt = match client.prepare(&sql).await {
                        Ok(s) => s,
                        Err(e) => return Err(WriteBackError::Backend(e.to_string())),
                    };
                    if let Err(msg) = coerce_params(&stmt, &mut ins_values) {
                        return Err(WriteBackError::InvalidPayload(msg));
                    }
                    let ins_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        ins_values.iter().map(SqlValue::as_tosql).collect();
                    match client.execute(&stmt, &ins_params).await {
                        Ok(rows) => {
                            *guard = Some(client);
                            // A fresh insert always affects one row; 0 rows here
                            // can ONLY be the guard firing on a pk owned by a
                            // different tenant — reject explicitly (same rule as
                            // the clobber path's rows == 0 check).
                            if rows == 0 {
                                return Err(WriteBackError::Forbidden(
                                    CROSS_TENANT_REJECTION.to_string(),
                                ));
                            }
                            Ok(())
                        }
                        Err(e) => Err(WriteBackError::Backend(e.to_string())),
                    }
                }
            }
        }

        /// ADR-0030 addendum: merge a PN-Counter payload per-replica elementwise
        /// max into the configured JSONB column (read-modify-write with the
        /// pool guard HELD across the whole RMW — single writer per row,
        /// audit M4).
        /// Mirrors `or_set_merge` but uses the counter merge primitive —
        /// including the tenant scoping (scoped SELECT, principal-stamped
        /// INSERT, guarded ON CONFLICT → `Forbidden` on cross-tenant pk).
        async fn counter_merge(
            &self,
            table: &str,
            pk: &str,
            col: &str,
            payload_json: &str,
            tenant: Option<TenantScope<'_>>,
        ) -> Result<(), WriteBackError> {
            if let Err(bad) = validate_ident(col) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad counter column identifier: {bad}"
                )));
            }
            if let Some(t) = tenant {
                if let Err(bad) = validate_ident(t.column) {
                    return Err(WriteBackError::InvalidPayload(format!(
                        "bad tenant column identifier: {bad}"
                    )));
                }
            }
            let quoted_table = quote_ident(table);
            let quoted_pk = quote_ident(PK_COLUMN);
            let quoted_col = quote_ident(col);
            let pk_value = SqlValue::from_pk(pk);

            // Hold the pool guard across the WHOLE read-modify-write so a
            // concurrent same-row merge serializes behind us instead of
            // opening a second connection and reading the same pre-merge
            // state. On error the slot stays None (client dropped with the
            // guard); the next call reconnects — same as drop_client.
            let mut guard = self.client.lock().await;
            let client = match guard.take() {
                Some(c) => c,
                None => crate::pg_connect::pg_connect_bounded(&self.pg_url)
                    .await
                    .map_err(WriteBackError::Backend)?,
            };
            let existing: Option<String> = match tenant {
                None => {
                    let select_sql = format!(
                        "SELECT {quoted_col}::text FROM {quoted_table} WHERE {quoted_pk} = $1"
                    );
                    let sel_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        vec![pk_value.as_tosql()];
                    match client.query_opt(&select_sql, &sel_params).await {
                        Ok(Some(row)) => row.get::<_, Option<String>>(0),
                        Ok(None) => None,
                        Err(e) => {
                            return Err(WriteBackError::Backend(e.to_string()));
                        }
                    }
                }
                Some(t) => {
                    let quoted_tcol = quote_ident(t.column);
                    let select_sql = format!(
                        "SELECT {quoted_col}::text FROM {quoted_table} \
                         WHERE {quoted_pk} = $1 AND {quoted_tcol} = $2"
                    );
                    let mut sel_values =
                        vec![SqlValue::from_pk(pk), SqlValue::from_scalar(t.value)];
                    let stmt = match client.prepare(&select_sql).await {
                        Ok(s) => s,
                        Err(e) => return Err(WriteBackError::Backend(e.to_string())),
                    };
                    if let Err(msg) = coerce_params(&stmt, &mut sel_values) {
                        return Err(WriteBackError::InvalidPayload(msg));
                    }
                    let sel_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        sel_values.iter().map(SqlValue::as_tosql).collect();
                    match client.query_opt(&stmt, &sel_params).await {
                        Ok(Some(row)) => row.get::<_, Option<String>>(0),
                        Ok(None) => None,
                        Err(e) => {
                            return Err(WriteBackError::Backend(e.to_string()));
                        }
                    }
                }
            };
            let existing_bytes = existing.as_deref().map_or(&b""[..], str::as_bytes);
            let merged =
                nostos_domain::merge_counter_or_lww(existing_bytes, payload_json.as_bytes());
            let merged_value: serde_json::Value =
                serde_json::from_slice(&merged).unwrap_or(serde_json::Value::Null);
            let col_value = json_value_to_sql(&merged_value);

            match tenant {
                None => {
                    let sql = format!(
                        "INSERT INTO {quoted_table} ({quoted_pk}, {quoted_col}) \
                         VALUES ($1, $2) \
                         ON CONFLICT ({quoted_pk}) DO UPDATE SET {quoted_col} = EXCLUDED.{quoted_col}"
                    );
                    let ins_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        vec![pk_value.as_tosql(), col_value.as_tosql()];
                    match client.execute(&sql, &ins_params).await {
                        Ok(_) => {
                            *guard = Some(client);
                            Ok(())
                        }
                        Err(e) => Err(WriteBackError::Backend(e.to_string())),
                    }
                }
                Some(t) => {
                    let quoted_tcol = quote_ident(t.column);
                    let sql = format!(
                        "INSERT INTO {quoted_table} ({quoted_pk}, {quoted_col}, {quoted_tcol}) \
                         VALUES ($1, $2, $3) \
                         ON CONFLICT ({quoted_pk}) DO UPDATE SET {quoted_col} = EXCLUDED.{quoted_col} \
                         WHERE {quoted_table}.{quoted_tcol} = EXCLUDED.{quoted_tcol}"
                    );
                    let mut ins_values = vec![
                        SqlValue::from_pk(pk),
                        col_value,
                        SqlValue::from_scalar(t.value),
                    ];
                    let stmt = match client.prepare(&sql).await {
                        Ok(s) => s,
                        Err(e) => return Err(WriteBackError::Backend(e.to_string())),
                    };
                    if let Err(msg) = coerce_params(&stmt, &mut ins_values) {
                        return Err(WriteBackError::InvalidPayload(msg));
                    }
                    let ins_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        ins_values.iter().map(SqlValue::as_tosql).collect();
                    match client.execute(&stmt, &ins_params).await {
                        Ok(rows) => {
                            *guard = Some(client);
                            // 0 rows ⟺ the guard fired on a pk owned by a
                            // different tenant — reject explicitly.
                            if rows == 0 {
                                return Err(WriteBackError::Forbidden(
                                    CROSS_TENANT_REJECTION.to_string(),
                                ));
                            }
                            Ok(())
                        }
                        Err(e) => Err(WriteBackError::Backend(e.to_string())),
                    }
                }
            }
        }
    }

    #[async_trait]
    impl WriteBack for PgWriteBack {
        async fn upsert(
            &self,
            table: &str,
            pk: &str,
            payload_json: &str,
            tenant: Option<TenantScope<'_>>,
        ) -> Result<(), WriteBackError> {
            // 1. ALLOWLIST FIRST. A table not in NOSTOS_WRITE_TABLES can never
            //    reach the SQL builder.
            if !self.allowlist.contains(table) {
                return Err(WriteBackError::TableNotAllowed(table.to_string()));
            }

            // 2. Validate the table identifier (the allowlist check above is
            //    exact-match against operator-supplied names, so this is
            //    belt-and-braces — but it's cheap and keeps the invariant that
            //    nothing reaches quote_ident() unvalidated).
            if let Err(bad) = validate_ident(table) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad table identifier: {bad}"
                )));
            }

            // ADR-0030 slice 3: OR-set tables merge element-wise into a configured
            // JSONB column instead of clobbering, so concurrent client adds
            // converge server-side. Tenant-scoped when a scope is active: the
            // merge reads/stamps/guards the tenant column exactly like the
            // clobber path below (ADR-0018 extended to the merge paths).
            if let Some(col) = self.or_set_columns.get(table) {
                return self
                    .or_set_merge(table, pk, col.as_str(), payload_json, tenant)
                    .await;
            }

            // ADR-0030 addendum: PN-Counter tables merge per-replica elementwise
            // max into a configured JSONB column, so concurrent client increments
            // converge server-side (state-based CRDT — no server-side counter
            // race). Tenant-scoped the same way, mirroring the OR-set path.
            if let Some(col) = self.counter_columns.get(table) {
                return self
                    .counter_merge(table, pk, col.as_str(), payload_json, tenant)
                    .await;
            }

            // 3. Parse + validate the payload. Must be a JSON object; every key
            //    (a column name) must pass the identifier regex.
            let mut payload: serde_json::Value = serde_json::from_str(payload_json)
                .map_err(|e| WriteBackError::InvalidPayload(format!("not JSON: {e}")))?;
            let obj = payload.as_object_mut().ok_or_else(|| {
                WriteBackError::InvalidPayload("payload must be a JSON object".to_string())
            })?;

            // 3b. ADR-0018: force-stamp the tenant column with the principal's
            //     tenant value — overwriting any client-supplied value for that
            //     key. This is NOT a client-attested field once tenant scoping
            //     is active; it becomes just another column in the INSERT/ON
            //     CONFLICT SET list below, so it flows through the same
            //     identifier-regex + parameterized-value path as every other
            //     column (no special-casing of the SQL builder needed).
            //     ponytail: assumes `tenant.column != PK_COLUMN` (an operator
            //     misconfiguring `NOSTOS_TENANT_COLUMN=id` — not attacker-
            //     reachable — would make the stamped value get filtered back
            //     out by the `!= PK_COLUMN` guard below, so the ON CONFLICT
            //     guard's `rows == 0` check could misfire as a false
            //     `Forbidden`). Not validated; no design partner has hit it.
            stamp_tenant_column(obj, tenant);

            // Order columns deterministically (sorted) so the same payload
            // always builds the same SQL — reproducible + cacheable. The pk
            // column ("id") is bound separately as $1 (from the `pk` arg, the
            // canonical source) and is EXCLUDED from the payload columns — a
            // payload that repeats it would otherwise produce a duplicate
            // column in the INSERT list. If the payload disagrees with `pk`,
            // the bound `pk` wins (LWW by WAL order, single writer per row).
            let mut columns: Vec<&String> =
                obj.keys().filter(|k| k.as_str() != PK_COLUMN).collect();
            columns.sort_unstable();
            for col in &columns {
                if let Err(bad) = validate_ident(col) {
                    return Err(WriteBackError::InvalidPayload(format!(
                        "bad column identifier: {bad}"
                    )));
                }
            }

            // 4. Build the statement. Identifiers are quoted (validated above);
            //    values are parameters ($1..$n) — NEVER interpolated.
            //
            //    Param layout: $1 = pk value, $2..$n = column values in sorted
            //    order. The INSERT lists "id" + columns; ON CONFLICT("id")
            //    updates every non-pk column from EXCLUDED.
            //
            //    Binding: each value's Rust type is inferred from its JSON
            //    shape (uuid → Uuid, bool → bool, int → i64, object/array →
            //    serde_json::Value [jsonb], else text). Postgres does NOT
            //    implicitly coerce text→uuid for parameters, so binding
            //    everything as text fails on typed columns (the plan's
            //    "text-cast binding" ponytail was incorrect for uuid). Inferring
            //    from the value itself needs no schema registry — it's correct
            //    for the common types and falls back to text otherwise.
            //    ponytail: when a schema registry exists (ADR-0012 follow-on),
            //    bind by the column's declared type instead of inferring.
            let quoted_table = quote_ident(table);
            let quoted_pk = quote_ident(PK_COLUMN);

            // Build the column list + value placeholders. The INSERT includes
            // the pk column first, then the payload columns.
            let mut insert_cols = String::with_capacity(64);
            insert_cols.push_str(&quoted_pk);
            let mut placeholders = String::with_capacity(64);
            placeholders.push_str("$1");
            // Typed values for binding (one per column, in sorted order).
            let mut col_values: Vec<SqlValue> = Vec::with_capacity(columns.len());
            for (i, col) in columns.iter().enumerate() {
                let value = obj.get(*col).map_or(SqlValue::Null, json_value_to_sql);
                col_values.push(value);
                insert_cols.push(',');
                insert_cols.push_str(&quote_ident(col));
                placeholders.push(',');
                // +2 because $1 is the pk; columns start at $2.
                let _ = write!(placeholders, "${}", i + 2);
            }

            // ON CONFLICT DO UPDATE SET "col"=EXCLUDED."col", ... for each
            // payload column (NOT the pk — the pk is the conflict target).
            //
            // ADR-0018 tenant guard: when `tenant` is `Some`, the tenant column
            // is always present in `columns` (stamped in step 3b above), so
            // `conflict_sets` is never empty here — the DO NOTHING branch below
            // is unreachable under tenant scoping. We add
            // `WHERE "table"."tenant_col" = EXCLUDED."tenant_col"` to the DO
            // UPDATE: since EXCLUDED's tenant column is always the PRINCIPAL's
            // tenant value (never the client's own claim), this guard reads as
            // "only update if the row ALREADY belongs to this principal's
            // tenant" — a conflict against a row owned by a different tenant
            // leaves that row untouched (0 rows affected), which we detect
            // below and turn into an explicit `Forbidden` rejection rather
            // than a silent ownership change. The existing-row side MUST be
            // qualified with the table name — Postgres rejects a bare
            // `"tenant_col" = EXCLUDED."tenant_col"` as an ambiguous column
            // reference (it can't tell which side of the comparison the bare
            // name refers to).
            let mut conflict_sets: Vec<String> = Vec::with_capacity(columns.len());
            for col in &columns {
                let q = quote_ident(col);
                conflict_sets.push(format!("{q}=EXCLUDED.{q}"));
            }
            let on_conflict = if conflict_sets.is_empty() {
                // Payload had only... nothing? No columns to update. A pure-pk
                // upsert is a no-op ON CONFLICT DO NOTHING. (Never reached when
                // `tenant` is `Some` — see the guard note above.)
                "ON CONFLICT DO NOTHING".to_string()
            } else {
                let guard = tenant
                    .map(|t| {
                        let q = quote_ident(t.column);
                        format!(" WHERE {quoted_table}.{q} = EXCLUDED.{q}")
                    })
                    .unwrap_or_default();
                format!(
                    "ON CONFLICT ({quoted_pk}) DO UPDATE SET {sets}{guard}",
                    sets = conflict_sets.join(", ")
                )
            };

            let sql = format!(
                "INSERT INTO {quoted_table} ({insert_cols}) VALUES ({placeholders}) {on_conflict}"
            );

            // 5. Execute with the pk + column values as parameters. The pk is
            //    the FIRST parameter ($1); column values follow in sorted order.
            //    Each SqlValue boxes its concrete ToSql type; we collect
            //    `&dyn ToSql` references into the slice tokio-postgres wants.
            let pk_value = SqlValue::from_pk(pk);
            let mut all_values: Vec<SqlValue> = Vec::with_capacity(1 + col_values.len());
            all_values.push(pk_value);
            all_values.extend(col_values);
            let client = self.client().await?;
            // Prepare first: the statement's server-reported parameter OIDs
            // are the declared column types — correct the shape-inferred
            // binds against them (i64→int4, "YYYY-MM-DD"→date, …) before
            // executing. Without this, any int4/date/float4 column rejects
            // the write client-side (ADR-0012 follow-on).
            let stmt = match client.prepare(&sql).await {
                Ok(s) => s,
                Err(e) => {
                    self.drop_client().await;
                    return Err(WriteBackError::Backend(e.to_string()));
                }
            };
            if let Err(msg) = coerce_params(&stmt, &mut all_values) {
                self.return_client(client).await;
                return Err(WriteBackError::InvalidPayload(msg));
            }
            let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                all_values.iter().map(SqlValue::as_tosql).collect();
            let result = client.execute(&stmt, &params).await;
            match result {
                Ok(rows) => {
                    self.return_client(client).await;
                    // A fresh insert (no conflict) always affects exactly one
                    // row regardless of the guard, so 0 rows here can ONLY
                    // happen when tenant scoping is active AND the ON CONFLICT
                    // WHERE guard fired — i.e. the pk already exists under a
                    // DIFFERENT tenant. Reject explicitly (ADR-0018): the
                    // client's write did not take effect and must not be told
                    // otherwise.
                    if tenant.is_some() && rows == 0 {
                        return Err(WriteBackError::Forbidden(
                            CROSS_TENANT_REJECTION.to_string(),
                        ));
                    }
                    Ok(())
                }
                Err(e) => {
                    self.drop_client().await;
                    Err(WriteBackError::Backend(e.to_string()))
                }
            }
        }

        async fn delete(
            &self,
            table: &str,
            pk: &str,
            tenant: Option<TenantScope<'_>>,
        ) -> Result<(), WriteBackError> {
            // 1. ALLOWLIST FIRST.
            if !self.allowlist.contains(table) {
                return Err(WriteBackError::TableNotAllowed(table.to_string()));
            }
            // 2. Validate the table identifier (belt-and-braces).
            if let Err(bad) = validate_ident(table) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad table identifier: {bad}"
                )));
            }

            let quoted_table = quote_ident(table);
            let quoted_pk = quote_ident(PK_COLUMN);

            // 3a. No tenant scoping active: unchanged v1 behavior. pk bound as
            //     $1 (NEVER interpolated), typed by inference (uuid pk → Uuid)
            //     then coerced to the statement's declared param type — a
            //     uuid-shaped pk in a TEXT column must bind as text (same
            //     coercion as the upsert path; without it tokio-postgres
            //     rejects the bind client-side with "error serializing
            //     parameter 0" — see e2e_pg_writeback_delete_text_pk).
            //     A missing row is success (idempotent) — Postgres's DELETE
            //     returns 0 rows affected, which is not an error.
            let Some(scope) = tenant else {
                let sql = format!("DELETE FROM {quoted_table} WHERE {quoted_pk} = $1");
                let client = self.client().await?;
                let mut values = vec![SqlValue::from_pk(pk)];
                let stmt = match client.prepare(&sql).await {
                    Ok(s) => s,
                    Err(e) => {
                        self.drop_client().await;
                        return Err(WriteBackError::Backend(e.to_string()));
                    }
                };
                if let Err(msg) = coerce_params(&stmt, &mut values) {
                    self.return_client(client).await;
                    return Err(WriteBackError::InvalidPayload(msg));
                }
                let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    values.iter().map(SqlValue::as_tosql).collect();
                return match client.execute(&stmt, &params).await {
                    Ok(_) => {
                        self.return_client(client).await;
                        Ok(())
                    }
                    Err(e) => {
                        self.drop_client().await;
                        Err(WriteBackError::Backend(e.to_string()))
                    }
                };
            };

            // 3b. ADR-0018 tenant-scoped delete: the DELETE is constrained to
            //     `pk = $1 AND tenant_col = $2`, so it can NEVER remove a row
            //     belonging to a different tenant. But a bare 0-rows-affected
            //     result is ambiguous — it means EITHER "no such row" (keep the
            //     idempotent-delete contract: success) OR "that row exists, but
            //     under someone else's tenant" (must be a clear rejection, not
            //     a silent no-op — the plan's explicit ask). We distinguish the
            //     two with a single round-trip: a data-modifying CTE that
            //     deletes under the tenant guard, then an EXISTS check (no
            //     tenant filter) to see whether the pk is still present.
            //
            //     Trade-off (documented, not swept under a ponytail): the
            //     EXISTS check reveals to the caller whether a pk they already
            //     named exists AT ALL, even under another tenant. We accept
            //     this for v1 — the caller supplied the pk themselves, so this
            //     adds at most one bit of information beyond what an upsert
            //     conflict on the same pk would already reveal (see the upsert
            //     guard above). A stricter mode that always returns idempotent
            //     success would need its own config flag; no design partner has
            //     asked for it. See docs/adr/0018-write-path-tenant-enforcement.md.
            if let Err(bad) = validate_ident(scope.column) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad tenant column identifier: {bad}"
                )));
            }
            let quoted_tenant_col = quote_ident(scope.column);
            let sql = format!(
                "WITH deleted AS (\
                     DELETE FROM {quoted_table} WHERE {quoted_pk} = $1 AND {quoted_tenant_col} = $2 \
                     RETURNING 1\
                 ) \
                 SELECT (SELECT count(*) FROM deleted)::bigint AS deleted_count, \
                        EXISTS(SELECT 1 FROM {quoted_table} WHERE {quoted_pk} = $1) AS still_exists"
            );
            let client = self.client().await?;
            // Same prepare-then-coerce as the unscoped branch and the upsert
            // path: uuid-shaped pk / tenant values in TEXT columns must bind
            // as text, not as typed Uuids.
            let mut values = vec![SqlValue::from_pk(pk), SqlValue::from_scalar(scope.value)];
            let stmt = match client.prepare(&sql).await {
                Ok(s) => s,
                Err(e) => {
                    self.drop_client().await;
                    return Err(WriteBackError::Backend(e.to_string()));
                }
            };
            if let Err(msg) = coerce_params(&stmt, &mut values) {
                self.return_client(client).await;
                return Err(WriteBackError::InvalidPayload(msg));
            }
            let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                values.iter().map(SqlValue::as_tosql).collect();
            match client.query_one(&stmt, &params).await {
                Ok(row) => {
                    self.return_client(client).await;
                    let deleted_count: i64 = row.get(0);
                    if deleted_count > 0 {
                        return Ok(());
                    }
                    let still_exists: bool = row.get(1);
                    if still_exists {
                        return Err(WriteBackError::Forbidden(
                            CROSS_TENANT_REJECTION.to_string(),
                        ));
                    }
                    Ok(()) // idempotent: the row never existed
                }
                Err(e) => {
                    self.drop_client().await;
                    Err(WriteBackError::Backend(e.to_string()))
                }
            }
        }

        /// Patch (column-level UPDATE) — P3 PowerSync parity. Mirrors the
        /// upsert builder's trust-boundary discipline (allowlist → ident regex
        /// → parameterized values) but emits `UPDATE … SET … WHERE pk=$pk`
        /// instead of `INSERT … ON CONFLICT`. A patch NEVER inserts.
        async fn patch(
            &self,
            table: &str,
            pk: &str,
            payload_json: &str,
            tenant: Option<TenantScope<'_>>,
        ) -> Result<(), WriteBackError> {
            // 1. ALLOWLIST FIRST.
            if !self.allowlist.contains(table) {
                return Err(WriteBackError::TableNotAllowed(table.to_string()));
            }
            // 2. Validate the table identifier (belt-and-braces).
            if let Err(bad) = validate_ident(table) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad table identifier: {bad}"
                )));
            }

            // 3. Parse + validate the payload (same path as upsert). Must be a
            //    JSON object; every key (a column name) passes the identifier
            //    regex before any SQL is built.
            let mut payload: serde_json::Value = serde_json::from_str(payload_json)
                .map_err(|e| WriteBackError::InvalidPayload(format!("not JSON: {e}")))?;
            let obj = payload.as_object_mut().ok_or_else(|| {
                WriteBackError::InvalidPayload("payload must be a JSON object".to_string())
            })?;

            // 3b. ADR-0018 force-stamp. CRITICAL for Patch: the WHERE in an
            //     UPDATE evaluates against the PRE-update row, so without this
            //     stamp a client payload containing `"org_id":"attacker"` would
            //     get applied by the SET clause and mutate the row's tenant
            //     ownership. Force-stamping overwrites any client value with
            //     the principal's real tenant; the tenant-guarded WHERE then
            //     constrains the update to rows already in this tenant, so the
            //     tenant column is set to the value it already had (a no-op on
            //     ownership). The client cannot mutate tenant ownership OR
            //     reach another tenant's row.
            stamp_tenant_column(obj, tenant);

            // Collect non-pk columns, sorted for deterministic SQL. The pk
            // column ("id") is bound separately in the WHERE and is NEVER in
            // the SET clause (mutating a pk is out of scope; v1 convention).
            let mut columns: Vec<&String> =
                obj.keys().filter(|k| k.as_str() != PK_COLUMN).collect();
            columns.sort_unstable();
            for col in &columns {
                if let Err(bad) = validate_ident(col) {
                    return Err(WriteBackError::InvalidPayload(format!(
                        "bad column identifier: {bad}"
                    )));
                }
            }
            // A patch with nothing to set is meaningless — reject it rather
            // than emit invalid SQL. Under tenant scoping the stamp above
            // guarantees the tenant column is present, so this fires only for
            // a genuinely empty payload (or a payload that carried only the pk).
            if columns.is_empty() {
                return Err(WriteBackError::InvalidPayload(
                    "patch payload has no columns to set".to_string(),
                ));
            }

            let quoted_table = quote_ident(table);
            let quoted_pk = quote_ident(PK_COLUMN);

            // Build the SET clause: `"col"=$1, "col"=$2, ...` (sorted). Column
            // NAMES are validated + quoted; VALUES are bound as $1…$n — NEVER
            // interpolated (same safety property as upsert).
            let mut set_clause = String::with_capacity(64);
            let mut col_values: Vec<SqlValue> = Vec::with_capacity(columns.len());
            for (i, col) in columns.iter().enumerate() {
                let value = obj.get(*col).map_or(SqlValue::Null, json_value_to_sql);
                col_values.push(value);
                let q = quote_ident(col);
                if i > 0 {
                    set_clause.push_str(", ");
                }
                // $1..$n for the columns; the +1 makes the first column $1.
                let _ = write!(set_clause, "{q}=${}", i + 1);
            }

            let pk_value = SqlValue::from_pk(pk);

            // 4a. No tenant scoping: plain UPDATE, 0 rows = absent = idempotent
            //     success (mirrors delete-of-missing). pk binds after the
            //     column values ($n+1).
            let Some(scope) = tenant else {
                let pk_param = columns.len() + 1;
                let sql = format!(
                    "UPDATE {quoted_table} SET {set_clause} WHERE {quoted_pk} = ${pk_param}"
                );
                let client = self.client().await?;
                let mut all_values: Vec<SqlValue> = Vec::with_capacity(col_values.len() + 1);
                all_values.extend(col_values);
                all_values.push(pk_value);
                let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    all_values.iter().map(SqlValue::as_tosql).collect();
                return match client.execute(&sql, &params).await {
                    Ok(_) => {
                        self.return_client(client).await;
                        Ok(())
                    }
                    Err(e) => {
                        self.drop_client().await;
                        Err(WriteBackError::Backend(e.to_string()))
                    }
                };
            };

            // 4b. ADR-0018 tenant-scoped patch: the same data-modifying-CTE +
            //     EXISTS probe the tenant-scoped delete uses. The UPDATE is
            //     constrained to `pk=$pk AND tenant_col=$tenant`, so it can
            //     never touch another tenant's row; the EXISTS check
            //     disambiguates a 0-rows result into absent (idempotent
            //     success) vs. exists-under-different-tenant (Forbidden). Same
            //     existence-disclosure trade-off as delete — documented, not
            //     hidden.
            if let Err(bad) = validate_ident(scope.column) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad tenant column identifier: {bad}"
                )));
            }
            let quoted_tenant_col = quote_ident(scope.column);
            let tenant_value = SqlValue::from_scalar(scope.value);
            let pk_param = columns.len() + 1;
            let tenant_param = columns.len() + 2;
            let sql = format!(
                "WITH updated AS (\
                     UPDATE {quoted_table} SET {set_clause} \
                     WHERE {quoted_pk} = ${pk_param} AND {quoted_tenant_col} = ${tenant_param} \
                     RETURNING 1\
                 ) \
                 SELECT (SELECT count(*) FROM updated)::bigint AS updated_count, \
                        EXISTS(SELECT 1 FROM {quoted_table} WHERE {quoted_pk} = ${pk_param}) AS still_exists"
            );
            let client = self.client().await?;
            let mut all_values: Vec<SqlValue> = Vec::with_capacity(col_values.len() + 2);
            all_values.extend(col_values);
            all_values.push(pk_value);
            all_values.push(tenant_value);
            let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                all_values.iter().map(SqlValue::as_tosql).collect();
            match client.query_one(&sql, &params).await {
                Ok(row) => {
                    self.return_client(client).await;
                    let updated_count: i64 = row.get(0);
                    if updated_count > 0 {
                        return Ok(());
                    }
                    let still_exists: bool = row.get(1);
                    if still_exists {
                        return Err(WriteBackError::Forbidden(
                            CROSS_TENANT_REJECTION.to_string(),
                        ));
                    }
                    Ok(()) // idempotent: the row never existed
                }
                Err(e) => {
                    self.drop_client().await;
                    Err(WriteBackError::Backend(e.to_string()))
                }
            }
        }

        /// Atomic increment (ADR-0030 Decision 1). `payload_json` is
        /// `{"field":"<col>","delta":<i64>}`; emits
        /// `UPDATE {table} SET {field} = {field} + $1 WHERE id = $2`. Postgres
        /// serializes concurrent increments — no client read-modify-write, no lost
        /// update. Tenant-scoped variant adds the same `AND tenant_col = $t` guard
        /// + EXISTS probe as `patch` (0 rows → absent/idempotent vs.
        /// exists-under-different-tenant/Forbidden). The field may not be the pk
        /// column or, when tenant-scoped, the tenant column — incrementing either
        /// would corrupt identity/ownership.
        async fn increment(
            &self,
            table: &str,
            pk: &str,
            payload_json: &str,
            tenant: Option<TenantScope<'_>>,
        ) -> Result<(), WriteBackError> {
            // 1. ALLOWLIST FIRST (ADR-0013 trust boundary).
            if !self.allowlist.contains(table) {
                return Err(WriteBackError::TableNotAllowed(table.to_string()));
            }
            if let Err(bad) = validate_ident(table) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad table identifier: {bad}"
                )));
            }

            // 2. Parse {field, delta}. `field` is a column name (validated); `delta`
            //    is an integer. ponytail: i64 covers every real counter (pomodoro
            //    session/streak counts); generalize to f64 only if a fractional
            //    counter actually appears — none does today.
            let payload: serde_json::Value = serde_json::from_str(payload_json)
                .map_err(|e| WriteBackError::InvalidPayload(format!("not JSON: {e}")))?;
            let obj = payload.as_object().ok_or_else(|| {
                WriteBackError::InvalidPayload("payload must be a JSON object".to_string())
            })?;
            let field = obj
                .get("field")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    WriteBackError::InvalidPayload(
                        "increment needs {\"field\",\"delta\"}".to_string(),
                    )
                })?;
            let delta = obj
                .get("delta")
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| {
                    WriteBackError::InvalidPayload("increment delta must be an integer".to_string())
                })?;
            if let Err(bad) = validate_ident(field) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad column identifier: {bad}"
                )));
            }
            // Never allow incrementing the pk (would corrupt row identity).
            if field == PK_COLUMN {
                return Err(WriteBackError::InvalidPayload(
                    "cannot increment the primary-key column".to_string(),
                ));
            }

            let quoted_table = quote_ident(table);
            let quoted_field = quote_ident(field);
            let quoted_pk = quote_ident(PK_COLUMN);
            let delta_value = SqlValue::Int(delta);
            let pk_value = SqlValue::from_pk(pk);

            // 3a. No tenant scoping: plain UPDATE, 0 rows = absent = idempotent
            //     success (mirrors patch-of-missing). delta binds $1, pk $2.
            let Some(scope) = tenant else {
                let sql = format!(
                "UPDATE {quoted_table} SET {quoted_field} = {quoted_field} + $1 WHERE {quoted_pk} = $2"
            );
                let client = self.client().await?;
                let all_values: Vec<SqlValue> = vec![delta_value, pk_value];
                let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    all_values.iter().map(SqlValue::as_tosql).collect();
                return match client.execute(&sql, &params).await {
                    Ok(_) => {
                        self.return_client(client).await;
                        Ok(())
                    }
                    Err(e) => {
                        self.drop_client().await;
                        Err(WriteBackError::Backend(e.to_string()))
                    }
                };
            };

            // 3b. Tenant-scoped increment. Refuse to increment the tenant column
            //     itself (would orphan the row from its tenant on the next filtered
            //     read). Same CTE + EXISTS probe as patch.
            if field == scope.column {
                return Err(WriteBackError::InvalidPayload(
                    "cannot increment the tenant column".to_string(),
                ));
            }
            if let Err(bad) = validate_ident(scope.column) {
                return Err(WriteBackError::InvalidPayload(format!(
                    "bad tenant column identifier: {bad}"
                )));
            }
            let quoted_tenant_col = quote_ident(scope.column);
            let tenant_value = SqlValue::from_scalar(scope.value);
            let sql = format!(
                "WITH updated AS (\
                 UPDATE {quoted_table} SET {quoted_field} = {quoted_field} + $1 \
                 WHERE {quoted_pk} = $2 AND {quoted_tenant_col} = $3 \
                 RETURNING 1\
             ) \
             SELECT (SELECT count(*) FROM updated)::bigint AS updated_count, \
                    EXISTS(SELECT 1 FROM {quoted_table} WHERE {quoted_pk} = $2) AS still_exists"
            );
            let client = self.client().await?;
            let all_values: Vec<SqlValue> = vec![delta_value, pk_value, tenant_value];
            let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                all_values.iter().map(SqlValue::as_tosql).collect();
            match client.query_one(&sql, &params).await {
                Ok(row) => {
                    self.return_client(client).await;
                    let updated_count: i64 = row.get(0);
                    if updated_count > 0 {
                        return Ok(());
                    }
                    let still_exists: bool = row.get(1);
                    if still_exists {
                        return Err(WriteBackError::Forbidden(
                            CROSS_TENANT_REJECTION.to_string(),
                        ));
                    }
                    Ok(()) // idempotent: the row never existed
                }
                Err(e) => {
                    self.drop_client().await;
                    Err(WriteBackError::Backend(e.to_string()))
                }
            }
        }
    }

    /// A typed SQL bind value. Infers the Rust type from the JSON value's
    /// shape so tokio-postgres sends the right wire type (PG does NOT implicitly
    /// coerce text→uuid for parameters). The variants cover the common column
    /// types; anything unrecognized falls back to text.
    ///
    /// `Null` is distinct from a JSON null so the SQL bind is SQL NULL (not the
    /// text "null") — a nullable column accepts it, a NOT NULL column rejects
    /// it with a clear PG error.
    ///
    /// Shape-inferred first (JSON carries no column types), then *corrected*
    /// against the prepared statement's server-reported parameter OIDs by
    /// [`SqlValue::coerce_to`] — the ADR-0012 "bind by declared type" upgrade.
    /// The shape guess alone rejects real schemas client-side (i64 vs an
    /// `integer` column, a date string vs a `date` column: the atlet
    /// `sessions` table hit both).
    enum SqlValue {
        Null,
        Bool(bool),
        Int(i64),
        /// 32-bit integer — coerced from `Int` when the column is `int4`.
        Int4(i32),
        /// 16-bit integer — coerced from `Int` when the column is `int2`.
        Int2(i16),
        Float(f64),
        /// 32-bit float — coerced from `Float` when the column is `float4`.
        Float4(f32),
        Text(String),
        Uuid(uuid::Uuid),
        /// RFC3339 timestamp — bound as `chrono::DateTime<Utc>` so a
        /// `timestamptz`/`timestamp` column accepts it (a bare `String` is
        /// rejected client-side by tokio-postgres's extended-query bind).
        Timestamp(chrono::DateTime<chrono::Utc>),
        /// Calendar date — coerced from a `YYYY-MM-DD` text value when the
        /// column is `date`.
        Date(chrono::NaiveDate),
        /// Timezone-less timestamp — coerced from `Timestamp` when the column
        /// is `timestamp` (tokio-postgres rejects `DateTime<Utc>` there).
        NaiveTs(chrono::NaiveDateTime),
        Json(serde_json::Value),
    }

    impl SqlValue {
        /// Build the SqlValue for a client-facing scalar identifier string —
        /// either the primary-key value or, per ADR-0018, a tenant-scope
        /// value. v1 convention is that such scalars are uuids, so parse as
        /// one; if not, fall back to text and let PG complain with a clear
        /// type error.
        fn from_scalar(s: &str) -> Self {
            match uuid::Uuid::parse_str(s) {
                Ok(u) => SqlValue::Uuid(u),
                Err(_) => SqlValue::Text(s.to_string()),
            }
        }

        /// Build the SqlValue for a primary-key string (alias of
        /// [`Self::from_scalar`] kept for call-site clarity).
        fn from_pk(pk: &str) -> Self {
            Self::from_scalar(pk)
        }

        /// Borrow the value as `&dyn ToSql` for the params slice.
        fn as_tosql(&self) -> &(dyn tokio_postgres::types::ToSql + Sync) {
            // The NULL sentinel: tokio-postgres binds SQL NULL via
            // `Option::<T>::None` (None of any ToSql type binds as NULL). A
            // static `None::<&str>` gives a stable reference for the params
            // slice.
            static NULL: Option<&str> = None;

            match self {
                SqlValue::Null => &NULL,
                SqlValue::Bool(b) => b,
                SqlValue::Int(i) => i,
                SqlValue::Int4(i) => i,
                SqlValue::Int2(i) => i,
                SqlValue::Float(f) => f,
                SqlValue::Float4(f) => f,
                SqlValue::Text(s) => s,
                SqlValue::Uuid(u) => u,
                SqlValue::Timestamp(dt) => dt,
                SqlValue::Date(d) => d,
                SqlValue::NaiveTs(dt) => dt,
                // serde_json::Value impls ToSql → jsonb (via with-serde_json-1).
                SqlValue::Json(v) => v,
            }
        }

        /// Correct a shape-inferred value to the column's *declared* type as
        /// reported by the prepared statement (ADR-0012 follow-on: the server
        /// catalog is the schema registry). Only lossless, unambiguous
        /// corrections are made; anything else is left for Postgres to reject
        /// with a clear error. This is what lets a JSON `30` land in an
        /// `integer` column and `"2026-08-07"` land in a `date` column instead
        /// of failing tokio-postgres's exact-type bind check.
        fn coerce_to(&mut self, ty: &tokio_postgres::types::Type) {
            use tokio_postgres::types::Type;
            let coerced = match (&*self, ty) {
                (SqlValue::Int(i), &Type::INT4) => i32::try_from(*i).ok().map(SqlValue::Int4),
                (SqlValue::Int(i), &Type::INT2) => i16::try_from(*i).ok().map(SqlValue::Int2),
                #[allow(clippy::cast_possible_truncation)]
                (SqlValue::Float(f), &Type::FLOAT4) => Some(SqlValue::Float4(*f as f32)),
                (SqlValue::Int(i), &Type::FLOAT8) =>
                {
                    #[allow(clippy::cast_precision_loss)]
                    Some(SqlValue::Float(*i as f64))
                }
                // An integral float (Dart `num` often serializes 100 as
                // 100.0) headed for an integer column: narrow when exact.
                #[allow(clippy::cast_possible_truncation)]
                (SqlValue::Float(f), &Type::INT4)
                    if f.fract() == 0.0
                        && *f >= f64::from(i32::MIN)
                        && *f <= f64::from(i32::MAX) =>
                {
                    Some(SqlValue::Int4(*f as i32))
                }
                #[allow(clippy::cast_possible_truncation)]
                (SqlValue::Float(f), &Type::INT8)
                    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 =>
                {
                    Some(SqlValue::Int(*f as i64))
                }
                (SqlValue::Text(s), &Type::DATE) => {
                    s.parse::<chrono::NaiveDate>().ok().map(SqlValue::Date)
                }
                (SqlValue::Timestamp(dt), &Type::TIMESTAMP) => {
                    Some(SqlValue::NaiveTs(dt.naive_utc()))
                }
                // A uuid-shaped or timestamp-shaped string headed for a plain
                // text column: undo the shape guess.
                (SqlValue::Uuid(u), &Type::TEXT | &Type::VARCHAR) => {
                    Some(SqlValue::Text(u.to_string()))
                }
                (SqlValue::Timestamp(dt), &Type::TEXT | &Type::VARCHAR) => {
                    Some(SqlValue::Text(dt.to_rfc3339()))
                }
                // A plain JSON string headed for a jsonb/json column.
                (SqlValue::Text(s), &Type::JSONB | &Type::JSON) => {
                    Some(SqlValue::Json(serde_json::Value::String(s.clone())))
                }
                _ => None,
            };
            if let Some(v) = coerced {
                *self = v;
            }
        }
    }

    /// Coerce every shape-inferred value to the prepared statement's declared
    /// parameter types (positional). See [`SqlValue::coerce_to`].
    ///
    /// Returns a readable rejection when a numeric value cannot fit its
    /// declared column type (e.g. `59999999940` into `int4`). Without this,
    /// the raw bind fails inside tokio-postgres with the opaque
    /// "error serializing parameter N" and the client retries a permanently
    /// doomed write.
    fn coerce_params(
        stmt: &tokio_postgres::Statement,
        values: &mut [SqlValue],
    ) -> Result<(), String> {
        use tokio_postgres::types::Type;
        for (idx, (value, ty)) in values.iter_mut().zip(stmt.params()).enumerate() {
            value.coerce_to(ty);
            // A residual wide value against a narrower column means the
            // coercion above declined (out of range / non-integral): reject
            // with the value and column type spelled out.
            let doomed = match (&*value, ty) {
                (SqlValue::Int(i), &Type::INT4 | &Type::INT2) => Some(i.to_string()),
                (SqlValue::Float(f), &Type::INT2 | &Type::INT4 | &Type::INT8) => {
                    Some(f.to_string())
                }
                _ => None,
            };
            if let Some(shown) = doomed {
                return Err(format!(
                    "parameter {} value {shown} does not fit column type {ty}",
                    idx + 1,
                ));
            }
        }
        Ok(())
    }

    /// Infer the bind type from a JSON value's shape:
    /// - string that parses as a UUID → `Uuid` (covers the common uuid-column
    ///   case; a uuid-typed column needs a real Uuid, not text).
    /// - bool / i64 / f64 → the matching scalar.
    /// - object / array → `Json` (→ jsonb column).
    /// - string (non-uuid) → text.
    /// - null → SQL NULL.
    fn json_value_to_sql(v: &serde_json::Value) -> SqlValue {
        match v {
            serde_json::Value::Null => SqlValue::Null,
            serde_json::Value::Bool(b) => SqlValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    SqlValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    SqlValue::Float(f)
                } else {
                    // Number too big for f64 — bind its text form.
                    SqlValue::Text(n.to_string())
                }
            }
            serde_json::Value::String(s) => {
                // Try uuid first (the common typed pk column), then an RFC3339
                // timestamp (created_at / timestamptz). tokio-postgres resolves
                // each parameter's type from the server (extended-query) and
                // rejects a `String` against TIMESTAMPTZ client-side, so a
                // timestamp-shaped value MUST bind as `DateTime<Utc>` or the
                // write returns ok:false ("error serializing parameter N").
                // Anything else stays text. Same shape-inference risk profile
                // as the uuid heuristic (a TEXT column holding an RFC3339
                // string would misbind); the schema-registry upgrade noted
                // below (OID-driven bind via the catalog) removes the guess.
                if let Ok(u) = uuid::Uuid::parse_str(s) {
                    SqlValue::Uuid(u)
                } else if let Some(dt) = parse_timestamp(s) {
                    SqlValue::Timestamp(dt)
                } else {
                    SqlValue::Text(s.clone())
                }
            }
            // object / array → jsonb.
            other => SqlValue::Json(other.clone()),
        }
    }

    /// Parse a strict RFC3339 / ISO8601 string into a UTC timestamp for typed
    /// binding to a `timestamptz` / `timestamp` column. `None` for anything
    /// that isn't RFC3339 — leaves text/uuid/other strings to their arms and
    /// avoids false positives on prose that happens to contain a date.
    fn parse_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    }

    /// ADR-0018: force-stamp the tenant column into an upsert payload with the
    /// principal's tenant value, overwriting any client-supplied value for
    /// that key. A no-op when `tenant` is `None` (no enforcement active).
    ///
    /// Pulled out as a pure function (no I/O) so the stamping logic is
    /// unit-testable without a database.
    fn stamp_tenant_column(
        obj: &mut serde_json::Map<String, serde_json::Value>,
        tenant: Option<TenantScope<'_>>,
    ) {
        if let Some(scope) = tenant {
            obj.insert(
                scope.column.to_string(),
                serde_json::Value::String(scope.value.to_string()),
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn coerce_to_narrows_integral_float_to_int4() {
            use tokio_postgres::types::Type;
            let mut v = SqlValue::Float(100.0);
            v.coerce_to(&Type::INT4);
            assert!(matches!(v, SqlValue::Int4(100)));
        }

        #[test]
        fn coerce_to_declines_out_of_range_int4() {
            use tokio_postgres::types::Type;
            // 59999999940 > i32::MAX — the real "DOMINGO" payload. Coercion
            // must decline (coerce_params then rejects with a readable
            // reason instead of the opaque tokio-postgres bind error).
            let mut v = SqlValue::Int(59_999_999_940);
            v.coerce_to(&Type::INT4);
            assert!(matches!(v, SqlValue::Int(59_999_999_940)));

            let mut f = SqlValue::Float(1.5);
            f.coerce_to(&Type::INT4);
            assert!(matches!(f, SqlValue::Float(_)));
        }

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
            // identifier must be rejected, so it can never reach quote_ident().
            assert!(validate_ident("a; DROP TABLE x").is_err());
            assert!(validate_ident("col\"--").is_err());
            assert!(validate_ident("Title").is_err()); // uppercase
            assert!(validate_ident("col name").is_err()); // space
            assert!(validate_ident("col;").is_err());
            assert!(validate_ident("col'name").is_err()); // quote
            assert!(validate_ident("1col").is_err()); // leading digit
            assert!(validate_ident("").is_err()); // empty
            assert!(validate_ident("col$").is_err()); // dollar
            assert!(validate_ident("café").is_err()); // non-ascii
        }

        #[test]
        fn quote_ident_doubles_embedded_quotes() {
            // The regex guarantees no `"` reaches here, but the doubling must
            // be correct regardless (defense-in-depth backstop).
            assert_eq!(quote_ident("tasks"), "\"tasks\"");
            // A hypothetical escaped quote — doubled, not backslashed.
            assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        }

        // -------------------------------------------------------------------
        // ADR-0018: tenant-column stamping (pure logic, no database needed).
        // -------------------------------------------------------------------

        #[test]
        fn stamp_tenant_column_noop_when_no_scope() {
            let mut obj = serde_json::Map::new();
            obj.insert("title".to_string(), serde_json::json!("hello"));
            stamp_tenant_column(&mut obj, None);
            assert_eq!(obj.len(), 1);
            assert!(!obj.contains_key("org_id"));
        }

        #[test]
        fn stamp_tenant_column_inserts_when_absent() {
            let mut obj = serde_json::Map::new();
            obj.insert("title".to_string(), serde_json::json!("hello"));
            let scope = TenantScope::new("org_id", "acme");
            stamp_tenant_column(&mut obj, Some(scope));
            assert_eq!(obj.get("org_id"), Some(&serde_json::json!("acme")));
        }

        #[test]
        fn stamp_tenant_column_overwrites_client_supplied_value() {
            // The security-critical case: a client that tries to claim a
            // DIFFERENT tenant in its own payload must be overridden, never
            // trusted (mirrors ADR-0011's read-side "server injects, client's
            // own value is dropped").
            let mut obj = serde_json::Map::new();
            obj.insert("org_id".to_string(), serde_json::json!("attacker-tenant"));
            let scope = TenantScope::new("org_id", "acme");
            stamp_tenant_column(&mut obj, Some(scope));
            assert_eq!(obj.get("org_id"), Some(&serde_json::json!("acme")));
        }
    }
}

#[cfg(feature = "pg")]
pub use pg::PgWriteBack;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allowlist_handles_empty_and_whitespace() {
        assert!(parse_allowlist("").is_empty());
        assert!(parse_allowlist("   ").is_empty());
        let al = parse_allowlist("tasks, users ,, notes");
        assert_eq!(al.len(), 3);
        assert!(al.contains("tasks"));
        assert!(al.contains("users"));
        assert!(al.contains("notes"));
        // whitespace trimmed
        assert!(al.contains("users"));
    }

    #[test]
    fn parse_or_set_columns_handles_empty_pairs_and_whitespace() {
        assert!(parse_or_set_columns("").is_empty());
        assert!(parse_or_set_columns("   ").is_empty());
        let cols = parse_or_set_columns("tasks:tags, notes:labels , events:attendees");
        assert_eq!(cols.len(), 3);
        assert_eq!(cols.get("tasks"), Some(&"tags".to_string()));
        assert_eq!(cols.get("notes"), Some(&"labels".to_string()));
        assert_eq!(cols.get("events"), Some(&"attendees".to_string()));
        // malformed entries (no colon / empty side) are skipped, not panics
        assert!(parse_or_set_columns("tasks, :tags, tasks:").is_empty());
        // duplicate table → last column wins (standard HashMap collect semantics)
        assert_eq!(
            parse_or_set_columns("tasks:tags,tasks:labels").get("tasks"),
            Some(&"labels".to_string())
        );
    }

    #[tokio::test]
    async fn nowriteback_always_errors_with_pg_required() {
        let wb = NoWriteBack::new();
        let upsert_err = wb.upsert("tasks", "1", "{}", None).await;
        match upsert_err {
            Err(WriteBackError::Backend(msg)) => {
                assert!(msg.contains("write-back requires pg replicator"));
            }
            other => panic!("upsert should error Backend, got {other:?}"),
        }
        let delete_err = wb.delete("tasks", "1", None).await;
        match delete_err {
            Err(WriteBackError::Backend(msg)) => {
                assert!(msg.contains("write-back requires pg replicator"));
            }
            other => panic!("delete should error Backend, got {other:?}"),
        }
        let patch_err = wb.patch("tasks", "1", "{}", None).await;
        match patch_err {
            Err(WriteBackError::Backend(msg)) => {
                assert!(msg.contains("write-back requires pg replicator"));
            }
            other => panic!("patch should error Backend, got {other:?}"),
        }
    }
}

// The identifier-validation tests live inside the `pg` module (the helpers are
// private to it). They run only under the `pg` feature — but the contract
// tests in `tests/ws_contract.rs` exercise the identifier boundary end-to-end
// regardless, so the no-pg build still has coverage of the trust boundary.
