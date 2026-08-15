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
//! tenant claim into a row. The REST stamping arrives with task 3.1
//! (`POST /push-tokens`); until then the store is exercised by tests only.
//!
//! Identity semantics: the token is the primary key (one token = one device =
//! one current account), so re-registering a token under a different account
//! MIGRATES the row — the previous principal stops being pushed on that
//! device. This is the structural defense against the ADR's "leaked
//! registration pushes the previous principal's data to the next user"
//! footgun; SDK sign-out deregistration (task 4.1) is the belt-and-braces.
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
use tokio_postgres::NoTls;

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
        let (client, conn) = tokio_postgres::connect(&self.pg_url, NoTls)
            .await
            .map_err(|e| TokenStoreError(format!("connect: {e}")))?;
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

    /// Drop the client slot — called after an error that may have killed the
    /// connection. The next call will reopen.
    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        *guard = None;
    }

    /// Register (or re-register) a device token for an account. Idempotent;
    /// a token re-registered under a different account migrates the row (see
    /// the module's identity-semantics note).
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
            INSERT INTO cairn_push_tokens (token, platform, account_id, tenant_id, updated_at) \
            VALUES ($1, $2, $3, $4, now()) \
            ON CONFLICT (token) DO UPDATE SET \
                platform = EXCLUDED.platform, \
                account_id = EXCLUDED.account_id, \
                tenant_id = EXCLUDED.tenant_id, \
                updated_at = now()";
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
