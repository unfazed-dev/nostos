//! Bounded lazy PG connect for the pool-of-one adapters (audit 2026-08-17
//! M8): every adapter used to call `tokio_postgres::connect` with no timeout
//! while holding its pool mutex — a blackholed PG (SYN-dropped) wedged all
//! users of that store for the OS TCP timeout. The PgStore security-hardening
//! template (15s connect bound + 30s session statement_timeout) is now the
//! one shared helper. The per-call error is a plain String so each adapter
//! maps it into its own error enum verbatim.

/// Bound for the TCP+auth handshake. The pool-of-one adapters serialize
/// every caller behind this wait, so it must fail fast.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Connect one client, bounded, and drive its socket on a detached task
/// (tokio-postgres drives the connection; dropping the `Client` closes it).
/// A session `statement_timeout` bounds every subsequent statement for the
/// same reason (every caller serializes behind the one connection).
///
/// # Errors
/// String messages (`connect timeout`, `connect: …`, `session config: …`) —
/// adapters wrap them in their own error enums.
pub(crate) async fn pg_connect_bounded(pg_url: &str) -> Result<tokio_postgres::Client, String> {
    let (client, conn) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_postgres::connect(pg_url, tokio_postgres::NoTls),
    )
    .await
    .map_err(|_| "connect timeout after 15s".to_string())?
    .map_err(|e| format!("connect: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute("SET statement_timeout = '30s'")
        .await
        .map_err(|e| format!("session config: {e}"))?;
    Ok(client)
}
