//! Real Nostos SQLite outbox/apply acceptance against the hosted Atlet Function.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use atlet_harness::auth::{sign_in, Credentials};
use clap::Parser;
use nostos_client::{appwrite::AppwriteDirectClient, SqliteStorage};
use nostos_core::{Outbox, PendingWrite, WriteOp};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    /// Ignored credentials file for the real Appwrite demo accounts.
    #[arg(long, default_value = "apps/atlet/.env.cloud")]
    credentials: PathBuf,
    /// In server mode, alternate gateway and direct devices on one journal.
    #[arg(long, default_value = "direct", value_parser = ["direct", "server"])]
    mode: String,
    #[arg(long)]
    gateway_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let credentials = Credentials::read(&args.credentials)?;
    let gateway_url = if args.mode == "server" {
        let url = args
            .gateway_url
            .as_deref()
            .or_else(|| credentials.get("NOSTOS_APPWRITE_GATEWAY_URL").ok())
            .context("server mode needs --gateway-url or NOSTOS_APPWRITE_GATEWAY_URL")?;
        if !url.starts_with("https://") {
            bail!("server mode requires an HTTPS gateway URL");
        }
        Some(url)
    } else {
        None
    };
    let endpoint = credentials.get("APPWRITE_ENDPOINT")?;
    let project = credentials.get("APPWRITE_PROJECT_ID")?;
    let http = reqwest::Client::new();
    let user_a = sign_in(
        &http,
        endpoint,
        project,
        credentials.get("ATLET_USER_A_EMAIL")?,
        credentials.get("ATLET_USER_A_PASSWORD")?,
        "atlet_user_a_demo",
    )
    .await?;
    let user_b = sign_in(
        &http,
        endpoint,
        project,
        credentials.get("ATLET_USER_B_EMAIL")?,
        credentials.get("ATLET_USER_B_PASSWORD")?,
        "atlet_user_b_demo",
    )
    .await?;
    let admin = sign_in(
        &http,
        endpoint,
        project,
        credentials.get("ATLET_ADMIN_EMAIL")?,
        credentials.get("ATLET_ADMIN_PASSWORD")?,
        "atlet_admin_demo",
    )
    .await?;
    let database = std::env::temp_dir().join(format!("atlet-native-{}.sqlite3", Uuid::new_v4()));
    let second_database =
        std::env::temp_dir().join(format!("atlet-native-{}.sqlite3", Uuid::new_v4()));
    let buyer_b_database =
        std::env::temp_dir().join(format!("atlet-native-{}.sqlite3", Uuid::new_v4()));
    let admin_database =
        std::env::temp_dir().join(format!("atlet-native-{}.sqlite3", Uuid::new_v4()));
    let session_id = format!("s{}", Uuid::new_v4().simple());
    let client = open(endpoint, project, gateway_url, &database)?;
    client.set_user("atlet_user_a_demo", &user_a).await?;
    if !client.needs_bootstrap() {
        bail!("new device did not require bootstrap");
    }
    client.sync().await.context("initial cloud bootstrap")?;
    // Keep three other devices online while A writes offline. They all share
    // one hosted journal but have independent SQLite stores and cursors.
    let second_a = open(endpoint, project, None, &second_database)?;
    second_a.set_user("atlet_user_a_demo", &user_a).await?;
    let buyer_b = open(endpoint, project, gateway_url, &buyer_b_database)?;
    buyer_b.set_user("atlet_user_b_demo", &user_b).await?;
    let admin_client = open(endpoint, project, None, &admin_database)?;
    admin_client.set_user("atlet_admin_demo", &admin).await?;
    tokio::try_join!(second_a.sync(), buyer_b.sync(), admin_client.sync())?;
    let payload = json!({
        "id":session_id,
        "title":"Atlet Nostos native offline smoke",
        "type":"reps",
        "metric":10,
        "unit":"reps",
        "streak":1,
        "occurred_on":"2026-09-26"
    });
    client.write_batch(&[PendingWrite {
        table: "sessions".into(),
        op: WriteOp::Upsert,
        pk: session_id.clone(),
        payload_json: Some(payload.to_string()),
    }])?;
    assert_session(&client, &session_id, true)?;
    if pending(&client)? != 1 {
        bail!("offline session was not queued");
    }
    drop(client);

    let client = open(endpoint, project, gateway_url, &database)?;
    client.set_user("atlet_user_a_demo", &user_a).await?;
    if client.needs_bootstrap() {
        bail!("reopened device lost its saved cloud horizon");
    }
    assert_session(&client, &session_id, true)?;
    if pending(&client)? != 1 {
        bail!("offline write did not survive SQLite reopen");
    }
    let outcome = client.sync().await.context("push offline session")?;
    if outcome.pushed != 1 || pending(&client)? != 0 {
        bail!("outbox was not acknowledged after cloud commit");
    }
    let row = session(&client, &session_id)?.context("server echo absent")?;
    if row["user_id"] != "atlet_user_a_demo" || row["server_committed_at"].is_null() {
        bail!("server-authoritative row was not applied");
    }

    tokio::try_join!(second_a.sync(), buyer_b.sync(), admin_client.sync())?;
    assert_session(&second_a, &session_id, true)?;
    assert_session(&buyer_b, &session_id, false)?;
    assert_session(&admin_client, &session_id, false)?;

    client.set_user("atlet_user_b_demo", &user_b).await?;
    assert_session(&client, &session_id, false)?;
    client.sync().await.context("customer B private pull")?;
    assert_session(&client, &session_id, false)?;
    client.set_user("atlet_user_a_demo", &user_a).await?;
    client.sync().await.context("customer A replay")?;
    assert_session(&client, &session_id, true)?;

    client.write_batch(&[PendingWrite {
        table: "sessions".into(),
        op: WriteOp::Delete,
        pk: session_id.clone(),
        payload_json: None,
    }])?;
    client.sync().await.context("journaled session cleanup")?;
    assert_session(&client, &session_id, false)?;
    tokio::try_join!(second_a.sync(), buyer_b.sync(), admin_client.sync())?;
    assert_session(&second_a, &session_id, false)?;
    let revoked_id = format!("s{}", Uuid::new_v4().simple());
    buyer_b.write_batch(&[PendingWrite {
        table: "sessions".into(),
        op: WriteOp::Upsert,
        pk: revoked_id.clone(),
        payload_json: Some(
            json!({
                "id":revoked_id,"title":"Revocation cache check","type":"reps",
                "metric":1,"unit":"reps","streak":1,"occurred_on":"2026-09-26"
            })
            .to_string(),
        ),
    }])?;
    buyer_b.sync().await.context("seed B private cache")?;
    assert_session(&buyer_b, &revoked_id, true)?;
    admin_client.write_batch(&[PendingWrite {
        table: "user_profiles".into(),
        op: WriteOp::Upsert,
        pk: "atlet_user_b_demo".into(),
        payload_json: Some(json!({"active":false}).to_string()),
    }])?;
    admin_client.sync().await.context("disable B")?;
    let denied = buyer_b.sync().await;
    // Restore the shared demo account even if the expected rejection fails.
    admin_client.write_batch(&[PendingWrite {
        table: "user_profiles".into(),
        op: WriteOp::Upsert,
        pk: "atlet_user_b_demo".into(),
        payload_json: Some(json!({"active":true}).to_string()),
    }])?;
    admin_client.sync().await.context("reactivate B")?;
    if !denied.is_err_and(|error| error.is_account_inactive()) {
        bail!("disabled customer did not get the explicit inactive response");
    }
    assert_session(&buyer_b, &revoked_id, false)?;
    if pending(&buyer_b)? != 0 || !buyer_b.needs_bootstrap() {
        bail!("disabled customer's local state was not reset");
    }
    buyer_b.set_user("atlet_user_b_demo", &user_b).await?;
    buyer_b.sync().await.context("reactivated B bootstrap")?;
    assert_session(&buyer_b, &revoked_id, true)?;
    buyer_b.write_batch(&[PendingWrite {
        table: "sessions".into(),
        op: WriteOp::Delete,
        pk: revoked_id.clone(),
        payload_json: None,
    }])?;
    buyer_b
        .sync()
        .await
        .context("cleanup B revocation fixture")?;
    client.sign_out().await?;
    second_a.sign_out().await?;
    buyer_b.sign_out().await?;
    admin_client.sign_out().await?;
    drop(client);
    drop(second_a);
    drop(buyer_b);
    drop(admin_client);
    for path in [
        &database,
        &second_database,
        &buyer_b_database,
        &admin_database,
    ] {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
    println!("Appwrite native smoke ({}): offline reopen, four-device convergence, private isolation, disabled-user cache wipe, cleanup passed", args.mode);
    Ok(())
}

fn open(
    endpoint: &str,
    project: &str,
    gateway_url: Option<&str>,
    path: &std::path::Path,
) -> Result<AppwriteDirectClient> {
    let storage = SqliteStorage::open(&path.to_string_lossy())?;
    Ok(if let Some(url) = gateway_url {
        AppwriteDirectClient::new_server(endpoint, project, "atlet_sync", url, storage)?
    } else {
        AppwriteDirectClient::new(endpoint, project, "atlet_sync", storage)?
    })
}

fn pending(client: &AppwriteDirectClient) -> Result<usize> {
    Ok(client
        .engine()
        .lock()
        .expect("engine lock")
        .storage()
        .pending()?
        .len())
}

fn session(client: &AppwriteDirectClient, id: &str) -> Result<Option<Value>> {
    let engine = client.engine().lock().expect("engine lock");
    let row = engine
        .storage()
        .rows_for("sessions")?
        .into_iter()
        .find(|(pk, _)| pk == id)
        .map(|(_, bytes)| serde_json::from_slice(&bytes))
        .transpose()?;
    Ok(row)
}

fn assert_session(client: &AppwriteDirectClient, id: &str, expected: bool) -> Result<()> {
    if session(client, id)?.is_some() != expected {
        bail!("session visibility mismatch for {id}");
    }
    Ok(())
}
