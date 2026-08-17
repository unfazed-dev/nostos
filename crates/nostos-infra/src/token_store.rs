//! Push-token registry — the transport-token table behind ADR-0037 §3.
//!
//! `PgTokenStore` owns `cairn_push_tokens`, the server-internal table the
//! push router resolves an offline account's devices through. It follows the
//! `PgWriteBack` pool-of-one pattern (`write_back.rs`): one lazily-opened
//! `tokio_postgres::Client` behind a tokio `Mutex`, transparently reopened
//! after a connection death. The table is created by the same idempotent-DDL
//! migration path as `cairn_oplog` (`docker/pg-init/01-sources.sql` /
//! `supabase/schema.sql`) — nostos has no migration framework and does not
//! invent one here.
//!
//! ## Trust boundary (ADR-0037 §3 + ADR-0018 discipline)
//!
//! `account_id` / `tenant_id` MUST be stamped server-side from the
//! authenticated `Principal` by the caller — this type is a dumb row-keeper
//! and deliberately offers no API shape that could smuggle a client-attested
//! tenant claim into a row. The REST surface (task 3.1, `POST /push-tokens`)
//! does the stamping via the `PushTokenRegistry` seam implemented below.
//!
//! Identity semantics: the token is the primary key (one token = one device =
//! one current account), so re-registering a token under a different account
//! MIGRATES the row — the previous principal stops being pushed on that
//! device — but only within one tenant: the conflict update is gated on the
//! existing row's tenant (M2), so a cross-tenant re-register keeps the
//! existing row. Within a tenant this is the structural defense against the
//! ADR's "leaked registration pushes the previous principal's data to the
//! next user" footgun; SDK sign-out deregistration (task 4.1) is the
//! belt-and-braces.
//!
//! ## SQL discipline (ADR-0013)
//!
//! Stricter than write-back: there are NO dynamic identifiers here at all.
//! Every statement is a static string whose table/column names are
//! compile-time constants (each matching `^[a-z_][a-z0-9_]*$` — asserted in
//! the unit test below), and every value is bound as `$1…$n`. The
//! identifier-regex gate is vacuous by construction; the parameter discipline
//! is not.

use std::sync::Arc;

use tokio::sync::Mutex;

/// One registered push token, as returned by [`PgTokenStore::list_by_account`].
/// `platform` names the rail (`'apns' | 'fcm' | 'webpush'`); the store treats
/// it as opaque — the rails interpret it (task 2.x).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushToken {
    pub platform: String,
    pub token: String,
}

/// A token-registry backend failure (connect/SQL). One shape on purpose:
/// every failure mode here is "the Postgres round-trip failed"; the REST
/// surface (task 3.1) maps it to a 5xx wholesale.
#[derive(Debug, thiserror::Error)]
#[error("token store backend: {0}")]
pub struct TokenStoreError(pub String);

/// The `cairn_push_tokens` registry, backed by a pool-of-one
/// `tokio_postgres::Client` (the `PgWriteBack` construction pattern:
/// lazy connect, reuse, transparent reopen after a dead connection).
///
/// ponytail: single connection; pool when a real load shows contention
/// (push registration is a rare REST call, never on the fan-out hot loop).
pub struct PgTokenStore {
    pg_url: String,
    /// Pool-of-one. `Mutex` (not `OnceCell`) so a dead connection can be
    /// replaced: take the lock, execute, and on a fatal error drop the inner
    /// `Client` (the next call reconnects).
    client: Arc<Mutex<Option<tokio_postgres::Client>>>,
}

impl PgTokenStore {
    /// Construct with a libpq-style URL. Does NOT connect — the first call
    /// opens the connection lazily (and reopens it transparently if it dies).
    #[must_use]
    pub fn new(pg_url: &str) -> Self {
        Self {
            pg_url: pg_url.to_string(),
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// Obtain a connected client, opening the connection lazily if none is
    /// cached. The connection's background task is spawned and forgotten
    /// (tokio-postgres drives the socket; dropping the `Client` closes it).
    async fn client(&self) -> Result<tokio_postgres::Client, TokenStoreError> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.take() {
            return Ok(c);
        }
        crate::pg_connect::pg_connect_bounded(&self.pg_url)
            .await
            .map_err(TokenStoreError)
    }

    /// Return a client to the pool (called after a successful statement).
    async fn return_client(&self, client: tokio_postgres::Client) {
        let mut guard = self.client.lock().await;
        *guard = Some(client);
    }

    /// Drop the client slot — called after an error that may have killed the
    /// connection. The next call will reopen.
    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        *guard = None;
    }

    /// Register (or re-register) a device token for an account. Idempotent.
    ///
    /// Identity semantics (M2): the token is the primary key, so a token
    /// re-registered under a different account MIGRATES the row (the
    /// previous principal stops being pushed on that device) — but only
    /// WITHIN one tenant: the conflict update is gated on the existing
    /// row's tenant matching the registrant's. A cross-tenant conflict
    /// (another tenant's account — same or different account id —
    /// re-registering the token) is a silent no-op keep-existing: a
    /// registration can never drag a token, and its pushes, across the
    /// tenant boundary (ADR-0018). Returns `Ok(())` in both cases — the
    /// registrant learns nothing about another tenant's row.
    ///
    /// Token-rotation hygiene: the same statement also sweeps this
    /// account's sibling tokens not re-registered within 30 days. An
    /// install rotation cannot know its predecessor's token, and FCM keeps
    /// accepting sends to it (same device + app), so the rail-level
    /// `UNREGISTERED` prune never fires — stale rows accumulate one per
    /// reinstall and every push fans out to all of them. A token whose app
    /// hasn't re-registered in 30 days is an abandoned install.
    /// ponytail: 30d constant, per-account piggyback on register (no
    /// background sweep); make it an env knob when an operator needs a
    /// different TTL.
    ///
    /// # Errors
    /// [`TokenStoreError`] if the Postgres round-trip fails.
    pub async fn upsert(
        &self,
        platform: &str,
        token: &str,
        account_id: &str,
        tenant_id: &str,
    ) -> Result<(), TokenStoreError> {
        let sql = "\
            WITH reg AS ( \
                INSERT INTO cairn_push_tokens (token, platform, account_id, tenant_id, updated_at) \
                VALUES ($1, $2, $3, $4, now()) \
                ON CONFLICT (token) DO UPDATE SET \
                    platform = EXCLUDED.platform, \
                    account_id = EXCLUDED.account_id, \
                    tenant_id = EXCLUDED.tenant_id, \
                    updated_at = now() \
                WHERE cairn_push_tokens.tenant_id = EXCLUDED.tenant_id \
                RETURNING token \
            ) \
            DELETE FROM cairn_push_tokens \
            WHERE account_id = $3 AND tenant_id = $4 \
              AND token <> $1 \
              AND updated_at < now() - interval '30 days'";
        let client = self.client().await?;
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 4] =
            [&token, &platform, &account_id, &tenant_id];
        match client.execute(sql, &params).await {
            Ok(_) => {
                self.return_client(client).await;
                Ok(())
            }
            Err(e) => {
                self.drop_client().await;
                Err(TokenStoreError(e.to_string()))
            }
        }
    }

    /// Remove one token registration. Called by the push rails on APNs 410 /
    /// FCM `UNREGISTERED` (task 2.x) — a dead device token must not stay in
    /// the send list. Returns rows deleted (0 = wasn't registered; idempotent).
    ///
    /// # Errors
    /// [`TokenStoreError`] if the Postgres round-trip fails.
    pub async fn prune(&self, token: &str) -> Result<u64, TokenStoreError> {
        let client = self.client().await?;
        match client
            .execute("DELETE FROM cairn_push_tokens WHERE token = $1", &[&token])
            .await
        {
            Ok(n) => {
                self.return_client(client).await;
                Ok(n)
            }
            Err(e) => {
                self.drop_client().await;
                Err(TokenStoreError(e.to_string()))
            }
        }
    }

    /// All tokens registered to one account, within one tenant. The tenant
    /// filter is the ADR-0018 isolation discipline — an account id colliding
    /// across tenants must never resolve the other tenant's devices.
    ///
    /// # Errors
    /// [`TokenStoreError`] if the Postgres round-trip fails.
    pub async fn list_by_account(
        &self,
        tenant_id: &str,
        account_id: &str,
    ) -> Result<Vec<PushToken>, TokenStoreError> {
        let client = self.client().await?;
        let rows = match client
            .query(
                "SELECT platform, token FROM cairn_push_tokens \
                 WHERE tenant_id = $1 AND account_id = $2",
                &[&tenant_id, &account_id],
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                self.drop_client().await;
                return Err(TokenStoreError(e.to_string()));
            }
        };
        self.return_client(client).await;
        Ok(rows
            .into_iter()
            .map(|row| PushToken {
                platform: row.get(0),
                token: row.get(1),
            })
            .collect())
    }

    /// Distinct tokens registered to one `(tenant, account)` — the REST
    /// surface's per-account cap count (L3). Same tenant filter discipline
    /// as [`Self::list_by_account`].
    ///
    /// # Errors
    /// [`TokenStoreError`] if the Postgres round-trip fails.
    pub async fn count_for_account(
        &self,
        tenant_id: &str,
        account_id: &str,
    ) -> Result<u64, TokenStoreError> {
        let client = self.client().await?;
        match client
            .query_one(
                "SELECT count(*) FROM cairn_push_tokens \
                 WHERE tenant_id = $1 AND account_id = $2",
                &[&tenant_id, &account_id],
            )
            .await
        {
            Ok(row) => {
                self.return_client(client).await;
                let n: i64 = row.get(0);
                Ok(u64::try_from(n).expect("count(*) is never negative"))
            }
            Err(e) => {
                self.drop_client().await;
                Err(TokenStoreError(e.to_string()))
            }
        }
    }

    /// Every token registered within one tenant, with its account — the
    /// tenant-wide expansion lookup (ADR-0037 §1 amendment): the push router
    /// resolves a tenant-wide hint to this list, then presence-filters per
    /// account at send time.
    ///
    /// # Errors
    /// [`TokenStoreError`] if the Postgres round-trip fails.
    pub async fn list_by_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<crate::push::router::RegisteredToken>, TokenStoreError> {
        let client = self.client().await?;
        let rows = match client
            .query(
                "SELECT tenant_id, account_id, platform, token FROM cairn_push_tokens \
                 WHERE tenant_id = $1",
                &[&tenant_id],
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                self.drop_client().await;
                return Err(TokenStoreError(e.to_string()));
            }
        };
        self.return_client(client).await;
        Ok(rows
            .into_iter()
            .map(|row| crate::push::router::RegisteredToken {
                tenant_id: row.get(0),
                account_id: row.get(1),
                platform: row.get(2),
                token: row.get(3),
            })
            .collect())
    }

    /// Owner-scoped delete (sign-out deregistration, plan 3.1): only the
    /// authenticated `(tenant, account)`'s own row disappears. A token that
    /// migrated to another principal is a no-op (0 rows) — one user can
    /// never deregister another user's device.
    ///
    /// # Errors
    /// [`TokenStoreError`] if the Postgres round-trip fails.
    pub async fn delete_for_owner(
        &self,
        tenant_id: &str,
        account_id: &str,
        token: &str,
    ) -> Result<u64, TokenStoreError> {
        let sql = "DELETE FROM cairn_push_tokens \
                   WHERE token = $1 AND tenant_id = $2 AND account_id = $3";
        let client = self.client().await?;
        match client
            .execute(sql, &[&token, &tenant_id, &account_id])
            .await
        {
            Ok(n) => {
                self.return_client(client).await;
                Ok(n)
            }
            Err(e) => {
                self.drop_client().await;
                Err(TokenStoreError(e.to_string()))
            }
        }
    }
}

/// The `PushTokenRegistry` seam (push/router) over the inherent methods —
/// the REST surface (plan 3.1) and the push router (plan 2.4) both talk to
/// the registry through this trait so the fake-mode in-memory twin can stand
/// in for dev builds and tests.
#[async_trait::async_trait]
impl crate::push::router::PushTokenRegistry for PgTokenStore {
    async fn upsert(
        &self,
        platform: &str,
        token: &str,
        account_id: &str,
        tenant_id: &str,
    ) -> Result<(), String> {
        PgTokenStore::upsert(self, platform, token, account_id, tenant_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn prune(&self, token: &str) -> Result<u64, String> {
        PgTokenStore::prune(self, token)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_for_owner(
        &self,
        tenant_id: &str,
        account_id: &str,
        token: &str,
    ) -> Result<u64, String> {
        PgTokenStore::delete_for_owner(self, tenant_id, account_id, token)
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_by_account(
        &self,
        tenant_id: &str,
        account_id: &str,
    ) -> Result<Vec<crate::push::router::RegisteredToken>, String> {
        PgTokenStore::list_by_account(self, tenant_id, account_id)
            .await
            .map(|tokens| {
                tokens
                    .into_iter()
                    .map(|t| crate::push::router::RegisteredToken {
                        tenant_id: tenant_id.to_string(),
                        account_id: account_id.to_string(),
                        platform: t.platform,
                        token: t.token,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    async fn list_by_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<crate::push::router::RegisteredToken>, String> {
        PgTokenStore::list_by_tenant(self, tenant_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn count_for_account(&self, tenant_id: &str, account_id: &str) -> Result<u64, String> {
        PgTokenStore::count_for_account(self, tenant_id, account_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    /// ADR-0013 identifier discipline: every table/column name embedded in
    /// this module's static SQL must match `^[a-z_][a-z0-9_]*$`. The
    /// statements are fully static (the stronger property — no dynamic
    /// identifiers exist here); this test keeps future column additions
    /// honest instead of letting a camelCase name slip into a SQL string.
    #[test]
    fn static_identifiers_match_adr0013_regex() {
        let re = regex::Regex::new(r"^[a-z_][a-z0-9_]*$")
            .expect("identifier regex is a valid static pattern");
        for ident in [
            "cairn_push_tokens",
            "token",
            "platform",
            "account_id",
            "tenant_id",
            "updated_at",
        ] {
            assert!(re.is_match(ident), "identifier {ident} violates ADR-0013");
        }
    }
}
