//! Repeatable, real-cloud check of the Appwrite journal's commit semantics.

use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::appwrite_client::{AppwriteClient, AppwriteError};

const DATABASE: &str = "atlet";
const MAX_ATTEMPTS: usize = 30;

/// Counts produced by one hosted-cloud probe run.
#[derive(Debug)]
pub struct ProbeResult {
    /// Product writes accepted by the cloud.
    pub creates: usize,
    /// Matching tombstones accepted by the cloud.
    pub deletes: usize,
    /// Clock conflicts that were retried without losing a mutation.
    pub conflicts: usize,
    /// Final committed sequence seen after cleanup.
    pub head: u64,
}

/// Concurrently create, pull, and delete real Appwrite products through the journal.
///
/// The rows carry a unique probe category and are removed with journaled
/// tombstones. A crash can leave named fixtures; the next run uses a new ID,
/// so it cannot mistake stale data for its own success.
///
/// # Errors
/// Returns a cloud API or protocol invariant failure.
pub async fn run(client: Arc<AppwriteClient>, writers: usize) -> Result<ProbeResult> {
    if !(1..=32).contains(&writers) {
        bail!("writers must be in 1..=32");
    }
    let baseline = clock(&client).await?;
    let run_id = Uuid::new_v4().simple().to_string();
    let mut tasks = JoinSet::new();
    for writer in 0..writers {
        let client = Arc::clone(&client);
        let run_id = run_id.clone();
        tasks.spawn(async move {
            let id = format!("p{}", Uuid::new_v4().simple());
            let row = json!({
                "name": format!("Nostos probe {run_id} writer {writer}"),
                "category": "nostos_probe",
                "price_cents": 1,
                "plant_based": true,
            });
            let (seq, conflicts) = mutate(&client, &id, Some(&row)).await?;
            Ok::<_, anyhow::Error>((id, seq, conflicts))
        });
    }
    let mut products = Vec::new();
    let mut conflicts = 0;
    while let Some(joined) = tasks.join_next().await {
        let (id, seq, count) = joined.context("probe writer panicked")??;
        products.push((id, seq));
        conflicts += count;
    }

    let first_head = clock(&client).await?;
    let first_feed = feed(&client, baseline, first_head).await?;
    assert_contiguous(&first_feed, baseline, first_head)?;
    let seen: HashSet<u64> = first_feed.iter().filter_map(change_seq).collect();
    for (id, seq) in &products {
        if !seen.contains(seq) {
            bail!("accepted product {id} absent from reconnect feed at sequence {seq}");
        }
        let row = client
            .get(&format!("tablesdb/{DATABASE}/tables/products/rows/{id}"))
            .await?
            .with_context(|| format!("accepted product {id} absent from cloud"))?;
        if row_data(&row)["category"] != "nostos_probe" {
            bail!("accepted product {id} has the wrong value");
        }
    }

    for (id, _) in &products {
        let (_, retries) = mutate(&client, id, None).await?;
        conflicts += retries;
    }
    let final_head = clock(&client).await?;
    let final_feed = feed(&client, first_head, final_head).await?;
    assert_contiguous(&final_feed, first_head, final_head)?;
    for (id, _) in &products {
        if client
            .get(&format!("tablesdb/{DATABASE}/tables/products/rows/{id}"))
            .await?
            .is_some()
        {
            bail!("deleted product {id} remains visible");
        }
        if !final_feed
            .iter()
            .any(|row| row_data(row)["pk"] == *id && row_data(row)["op"] == "delete")
        {
            bail!("deleted product {id} has no reconnect tombstone");
        }
    }
    Ok(ProbeResult {
        creates: products.len(),
        deletes: products.len(),
        conflicts,
        head: final_head,
    })
}

/// Journal-delete a legacy manual probe row, refusing to touch app data.
///
/// # Errors
/// Returns an error if the row is not a known manual probe fixture or if the
/// cloud transaction fails.
pub async fn cleanup_legacy(client: &AppwriteClient, id: &str) -> Result<u64> {
    if !id.starts_with("probe-p-") {
        bail!("legacy cleanup accepts only probe-p-* row IDs");
    }
    let row = client
        .get(&format!("tablesdb/{DATABASE}/tables/products/rows/{id}"))
        .await?
        .context("legacy probe product is absent")?;
    if row_data(&row)["category"] != "test" {
        bail!("legacy cleanup refuses product outside the test category");
    }
    let (seq, _) = mutate(client, id, None).await?;
    Ok(seq)
}

async fn mutate(
    client: &AppwriteClient,
    id: &str,
    product: Option<&Value>,
) -> Result<(u64, usize)> {
    let mutation_id = Uuid::new_v4().simple().to_string();
    let request_hash = hex::encode(Sha256::digest(
        format!(
            "{id}:{}",
            product.map_or_else(|| "delete".to_string(), Value::to_string)
        )
        .as_bytes(),
    ));
    for attempt in 0..MAX_ATTEMPTS {
        if let Some(existing) = client
            .get(&format!(
                "tablesdb/{DATABASE}/tables/sync_mutations/rows/{mutation_id}"
            ))
            .await?
        {
            let data = row_data(&existing);
            if data["actor_id"] != "probe" || data["request_hash"] != request_hash {
                bail!("mutation ID reused for a different request");
            }
            return Ok((
                data["seq"]
                    .as_u64()
                    .context("mutation ledger omitted seq")?,
                attempt,
            ));
        }
        let next = clock(client).await? + 1;
        let tx = client
            .post("tablesdb/transactions", &json!({"ttl": 60}))
            .await?;
        let tx_id = tx["$id"]
            .as_str()
            .context("transaction response omitted ID")?;
        let result = stage_and_commit(
            client,
            tx_id,
            next,
            id,
            product,
            &mutation_id,
            &request_hash,
        )
        .await;
        match result {
            Ok(()) => return Ok((next, attempt)),
            Err(error) if is_conflict(&error) => {
                let _ = client
                    .patch(
                        &format!("tablesdb/transactions/{tx_id}"),
                        &json!({"rollback": true}),
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(25 * (attempt as u64 + 1))).await;
            }
            Err(error) => {
                let _ = client
                    .patch(
                        &format!("tablesdb/transactions/{tx_id}"),
                        &json!({"rollback": true}),
                    )
                    .await;
                return Err(error);
            }
        }
    }
    bail!("Appwrite clock conflict did not settle after {MAX_ATTEMPTS} attempts")
}

async fn stage_and_commit(
    client: &AppwriteClient,
    tx_id: &str,
    seq: u64,
    id: &str,
    product: Option<&Value>,
    mutation_id: &str,
    request_hash: &str,
) -> Result<()> {
    client
        .patch(
            &format!("tablesdb/{DATABASE}/tables/sync_clock/rows/clock"),
            &json!({"data": {"head": seq}, "transactionId": tx_id}),
        )
        .await?;
    let op = if let Some(product) = product {
        client
            .post(
                &format!("tablesdb/{DATABASE}/tables/products/rows"),
                &json!({"rowId": id, "data": product, "permissions": [], "transactionId": tx_id}),
            )
            .await?;
        "upsert"
    } else {
        client
            .delete(&format!(
                "tablesdb/{DATABASE}/tables/products/rows/{id}?transactionId={tx_id}"
            ))
            .await?;
        "delete"
    };
    let row_json = product.map(|data| {
        let mut row = data.clone();
        row["id"] = json!(id);
        row.to_string()
    });
    client
        .post(
            &format!("tablesdb/{DATABASE}/tables/sync_changes/rows"),
            &json!({
                "rowId": format!("c{seq:019}"),
                "data": {
                    "seq": seq,
                    "table_name": "products",
                    "pk": id,
                    "op": op,
                    "row_json": row_json,
                    "audience": "public",
                },
                "permissions": [],
                "transactionId": tx_id,
            }),
        )
        .await?;
    client
        .post(
            &format!("tablesdb/{DATABASE}/tables/sync_mutations/rows"),
            &json!({
                "rowId": mutation_id,
                "data": {
                    "seq": seq,
                    "actor_id": "probe",
                    "request_hash": request_hash,
                    "created_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                },
                "permissions": [],
                "transactionId": tx_id,
            }),
        )
        .await?;
    client
        .patch(
            &format!("tablesdb/transactions/{tx_id}"),
            &json!({"commit": true}),
        )
        .await?;
    Ok(())
}

async fn clock(client: &AppwriteClient) -> Result<u64> {
    let row = client
        .get(&format!("tablesdb/{DATABASE}/tables/sync_clock/rows/clock"))
        .await?
        .context("Appwrite sync clock is missing")?;
    row_data(&row)["head"]
        .as_u64()
        .context("clock head is not an unsigned integer")
}

async fn feed(client: &AppwriteClient, after: u64, through: u64) -> Result<Vec<Value>> {
    let mut cursor = after;
    let mut out = Vec::new();
    while cursor < through {
        let mut url = reqwest::Url::parse("https://unused.invalid/")?;
        url.query_pairs_mut()
            .append_pair(
                "queries[]",
                &json!({"method":"greaterThan","attribute":"seq","values":[cursor]}).to_string(),
            )
            .append_pair(
                "queries[]",
                &json!({"method":"lessThanEqual","attribute":"seq","values":[through]}).to_string(),
            )
            .append_pair(
                "queries[]",
                &json!({"method":"orderAsc","attribute":"seq"}).to_string(),
            )
            .append_pair(
                "queries[]",
                &json!({"method":"limit","values":[100]}).to_string(),
            );
        let path = format!(
            "tablesdb/{DATABASE}/tables/sync_changes/rows?{}",
            url.query().context("queries missing")?
        );
        let response = client.get(&path).await?.context("change feed missing")?;
        let rows = response["rows"]
            .as_array()
            .context("change feed omitted rows")?;
        if rows.is_empty() {
            bail!("change feed stopped at {cursor} before committed head {through}");
        }
        for row in rows {
            cursor = change_seq(row).context("change row omitted sequence")?;
            out.push(row.clone());
        }
    }
    Ok(out)
}

fn assert_contiguous(rows: &[Value], after: u64, through: u64) -> Result<()> {
    let mut expected = after;
    for row in rows {
        expected += 1;
        if change_seq(row) != Some(expected) {
            bail!("change feed gap at sequence {expected}");
        }
    }
    if expected != through {
        bail!("change feed ended at {expected}, clock is {through}");
    }
    Ok(())
}

fn change_seq(row: &Value) -> Option<u64> {
    row_data(row)["seq"].as_u64()
}

fn row_data(row: &Value) -> &Value {
    row.get("data").unwrap_or(row)
}

fn is_conflict(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<AppwriteError>()
        .is_some_and(|api| api.is_status(StatusCode::CONFLICT))
}
