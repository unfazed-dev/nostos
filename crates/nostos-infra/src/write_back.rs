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

use std::collections::HashSet;

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
}

// ===========================================================================
// PgWriteBack — the real adapter (feature "pg").
// ===========================================================================
#[cfg(feature = "pg")]
mod pg {
    use async_trait::async_trait;
    use nostos_application::ports::{WriteBack, WriteBackError};
    use nostos_domain::TenantScope;
    use std::collections::HashSet;
    use std::fmt::Write as _; // ponytail: single write!() for the $n placeholder
    use std::sync::Arc;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;
    use tokio_postgres::NoTls;

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
    pub struct PgWriteBack {
        pg_url: String,
        allowlist: HashSet<String>,
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
                client: Arc::new(Mutex::new(None)),
            }
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
            // Open a fresh connection.
            let (client, conn) = tokio_postgres::connect(&self.pg_url, NoTls)
                .await
                .map_err(|e| WriteBackError::Backend(format!("connect: {e}")))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok(client)
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
            let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                all_values.iter().map(SqlValue::as_tosql).collect();
            let result = client.execute(&sql, &params).await;
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
                        return Err(WriteBackError::Forbidden(format!(
                            "row {pk} in {table} belongs to a different tenant"
                        )));
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
            //     $1 (NEVER interpolated), typed by inference (uuid pk → Uuid).
            //     A missing row is success (idempotent) — Postgres's DELETE
            //     returns 0 rows affected, which is not an error.
            let Some(scope) = tenant else {
                let sql = format!("DELETE FROM {quoted_table} WHERE {quoted_pk} = $1");
                let client = self.client().await?;
                let pk_value = SqlValue::from_pk(pk);
                let params: [&(dyn tokio_postgres::types::ToSql + Sync); 1] = [pk_value.as_tosql()];
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
            let pk_value = SqlValue::from_pk(pk);
            let tenant_value = SqlValue::from_scalar(scope.value);
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] =
                [pk_value.as_tosql(), tenant_value.as_tosql()];
            match client.query_one(&sql, &params).await {
                Ok(row) => {
                    self.return_client(client).await;
                    let deleted_count: i64 = row.get(0);
                    if deleted_count > 0 {
                        return Ok(());
                    }
                    let still_exists: bool = row.get(1);
                    if still_exists {
                        return Err(WriteBackError::Forbidden(format!(
                            "row {pk} in {table} belongs to a different tenant"
                        )));
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
    /// ponytail: when a schema registry exists (ADR-0012 follow-on), bind by
    /// the column's declared type instead of inferring from the value.
    enum SqlValue {
        Null,
        Bool(bool),
        Int(i64),
        Float(f64),
        Text(String),
        Uuid(uuid::Uuid),
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
                SqlValue::Float(f) => f,
                SqlValue::Text(s) => s,
                SqlValue::Uuid(u) => u,
                // serde_json::Value impls ToSql → jsonb (via with-serde_json-1).
                SqlValue::Json(v) => v,
            }
        }
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
                // Try uuid first (the most common typed column in the demo
                // schema). A string that isn't a uuid stays text.
                match uuid::Uuid::parse_str(s) {
                    Ok(u) => SqlValue::Uuid(u),
                    Err(_) => SqlValue::Text(s.clone()),
                }
            }
            // object / array → jsonb.
            other => SqlValue::Json(other.clone()),
        }
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
    }
}

// The identifier-validation tests live inside the `pg` module (the helpers are
// private to it). They run only under the `pg` feature — but the contract
// tests in `tests/ws_contract.rs` exercise the identifier boundary end-to-end
// regardless, so the no-pg build still has coverage of the trust boundary.
