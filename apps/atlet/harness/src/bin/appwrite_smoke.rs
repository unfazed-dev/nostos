//! Real Appwrite Auth and Function smoke, using three demo credentials.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use atlet_harness::auth::{sign_in, Credentials};
use clap::Parser;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    /// Ignored, mode 0600 credentials file created for the three demo accounts.
    #[arg(long, default_value = "apps/atlet/.env.cloud")]
    credentials: PathBuf,
    /// Delete one known admin smoke fixture through the journaled Function.
    #[arg(long)]
    cleanup_product_id: Option<String>,
    /// Complete a known paid test order through the journaled Function.
    #[arg(long)]
    fulfill_order_id: Option<String>,
    /// Exercise purchase, admin fulfilment, and private event delivery.
    /// The order remains as a demo history row in the hosted project.
    #[arg(long)]
    order_workflow: bool,
    /// Seed demo user profiles and verify admin user controls and visibility.
    #[arg(long)]
    users_workflow: bool,
}

struct Account {
    role: &'static str,
    jwt: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let secrets = Credentials::read(&args.credentials)?;
    let endpoint = secrets.get("APPWRITE_ENDPOINT")?;
    let project = secrets.get("APPWRITE_PROJECT_ID")?;
    let client = reqwest::Client::builder().build()?;
    let mut accounts = Vec::new();
    for (role, id, prefix) in [
        ("admin", "atlet_admin_demo", "ATLET_ADMIN"),
        ("customer_a", "atlet_user_a_demo", "ATLET_USER_A"),
        ("customer_b", "atlet_user_b_demo", "ATLET_USER_B"),
    ] {
        let jwt = sign_in(
            &client,
            endpoint,
            project,
            secrets.get(&format!("{prefix}_EMAIL"))?,
            secrets.get(&format!("{prefix}_PASSWORD"))?,
            id,
        )
        .await
        .with_context(|| format!("sign in {role}"))?;
        accounts.push(Account { role, jwt });
    }
    if let Some(product_id) = args.cleanup_product_id {
        Uuid::parse_str(&product_id).context("cleanup product ID must be a UUID")?;
        invoke(
            &client,
            endpoint,
            project,
            &accounts[0],
            "/sync/push",
            &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"products","pk":product_id,"op":"delete"}),
        )
        .await?;
        println!("Appwrite admin product fixture deleted through sync Function");
        return Ok(());
    }
    if let Some(order_id) = args.fulfill_order_id {
        Uuid::parse_str(&order_id).context("fulfilment order ID must be a UUID")?;
        for status in ["shipped", "delivered"] {
            invoke(
                &client,
                endpoint,
                project,
                &accounts[0],
                "/sync/push",
                &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"orders","pk":order_id,"op":"upsert","payload":{"status":status}}),
            )
            .await?;
        }
        println!("Appwrite demo order {order_id} delivered through sync Function");
        return Ok(());
    }
    if args.users_workflow {
        users_workflow(&client, endpoint, project, &accounts).await?;
        return Ok(());
    }
    let before = pull(&client, endpoint, project, &accounts[0], "0").await?;
    let after = before["head"]
        .as_str()
        .context("pull omitted head")?
        .to_string();
    for account in &accounts {
        let response = pull(&client, endpoint, project, account, &after).await?;
        if response["changes"].as_array().is_none() {
            bail!("{} pull omitted changes", account.role);
        }
    }

    let session_id = format!("s{}", Uuid::new_v4().simple());
    let write_id = Uuid::new_v4().simple().to_string();
    let mutation = json!({
        "mutation_id": write_id,
        "table": "sessions",
        "pk": session_id,
        "op": "upsert",
        "payload": {
            "title": "Atlet cloud smoke",
            "type": "reps",
            "metric": 10,
            "unit": "reps",
            "streak": 1,
            "occurred_on": "2026-09-26",
        }
    });
    let pushed = invoke(
        &client,
        endpoint,
        project,
        &accounts[1],
        "/sync/push",
        &mutation,
    )
    .await?;
    let seq = pushed["seq"].as_str().context("push omitted sequence")?;
    let replay = invoke(
        &client,
        endpoint,
        project,
        &accounts[1],
        "/sync/push",
        &mutation,
    )
    .await?;
    if replay["seq"] != seq || replay["duplicate"] != true {
        bail!("replayed mutation was applied twice");
    }
    let a = pull(&client, endpoint, project, &accounts[1], &after).await?;
    let b = pull(&client, endpoint, project, &accounts[2], &after).await?;
    let admin = pull(&client, endpoint, project, &accounts[0], &after).await?;
    if !has_pk(&a, &session_id) || has_pk(&b, &session_id) || has_pk(&admin, &session_id) {
        bail!("private session visibility failed");
    }
    let deleted = invoke(
        &client,
        endpoint,
        project,
        &accounts[1],
        "/sync/push",
        &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"sessions","pk":session_id,"op":"delete"}),
    )
    .await?;
    if deleted["seq"].as_str().is_none() {
        bail!("session cleanup failed");
    }
    let product_id = Uuid::new_v4().to_string();
    let product = json!({
        "mutation_id":Uuid::new_v4().simple().to_string(),
        "table":"products",
        "pk":product_id,
        "op":"upsert",
        "payload":{"name":"Atlet cloud smoke product","category":"Equipment","price_cents":1590,"plant_based":false}
    });
    invoke(
        &client,
        endpoint,
        project,
        &accounts[0],
        "/sync/push",
        &product,
    )
    .await?;
    let customer_view = pull(&client, endpoint, project, &accounts[2], &after).await?;
    if !has_pk(&customer_view, &product_id) {
        bail!("admin catalog write was not visible to a customer");
    }
    let refused = invoke(
        &client,
        endpoint,
        project,
        &accounts[2],
        "/sync/push",
        &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"products","pk":product_id,"op":"delete"}),
    )
    .await;
    if !refused.is_err_and(|error| error.to_string().contains("403")) {
        bail!("customer could modify the admin catalog");
    }
    invoke(
        &client,
        endpoint,
        project,
        &accounts[0],
        "/sync/push",
        &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"products","pk":product_id,"op":"delete"}),
    )
    .await?;
    if args.order_workflow {
        order_workflow(&client, endpoint, project, &accounts).await?;
    }
    println!(
        "Appwrite smoke: 3 real logins, private pull, idempotent push, admin catalog ACL, cleanup passed"
    );
    Ok(())
}

async fn users_workflow(
    client: &reqwest::Client,
    endpoint: &str,
    project: &str,
    accounts: &[Account],
) -> Result<()> {
    let before = pull(client, endpoint, project, &accounts[0], "0").await?;
    let after = before["head"].as_str().context("profile cursor missing")?;
    for (id, name) in [
        ("atlet_admin_demo", "Atlet Admin"),
        ("atlet_user_a_demo", "Atlet Customer A"),
        ("atlet_user_b_demo", "Atlet Customer B"),
    ] {
        invoke(
            client,
            endpoint,
            project,
            &accounts[0],
            "/sync/push",
            &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"user_profiles","pk":id,"op":"upsert","payload":{"display_name":name,"active":true}}),
        )
        .await?;
    }
    let admin = pull(client, endpoint, project, &accounts[0], after).await?;
    let customer = pull(client, endpoint, project, &accounts[1], after).await?;
    for id in ["atlet_admin_demo", "atlet_user_a_demo", "atlet_user_b_demo"] {
        if !has_pk(&admin, id) {
            bail!("admin cannot see user profile {id}");
        }
    }
    if !has_pk(&customer, "atlet_user_a_demo")
        || has_pk(&customer, "atlet_admin_demo")
        || has_pk(&customer, "atlet_user_b_demo")
    {
        bail!("customer user-profile isolation failed");
    }
    invoke(
        client,
        endpoint,
        project,
        &accounts[0],
        "/sync/push",
        &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"user_profiles","pk":"atlet_user_b_demo","op":"upsert","payload":{"active":false}}),
    )
    .await?;
    let denied = pull(client, endpoint, project, &accounts[2], after).await;
    // Always restore the demo account before reporting a failed assertion.
    invoke(
        client,
        endpoint,
        project,
        &accounts[0],
        "/sync/push",
        &json!({"mutation_id":Uuid::new_v4().simple().to_string(),"table":"user_profiles","pk":"atlet_user_b_demo","op":"upsert","payload":{"active":true}}),
    )
    .await?;
    if !denied.is_err_and(|error| error.to_string().contains("403")) {
        bail!("inactive customer could still pull");
    }
    pull(client, endpoint, project, &accounts[2], after).await?;
    println!(
        "Appwrite user workflow passed: three profiles, private visibility, disable/reactivate"
    );
    Ok(())
}

async fn order_workflow(
    client: &reqwest::Client,
    endpoint: &str,
    project: &str,
    accounts: &[Account],
) -> Result<()> {
    let before = pull(client, endpoint, project, &accounts[0], "0").await?;
    let after = before["head"].as_str().context("order cursor missing")?;
    let order_id = Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339();
    let items = json!([{"product_id":"smoke-product","name":"Smoke training kit","qty":1,"price_cents":1590}]);
    invoke(
        client,
        endpoint,
        project,
        &accounts[1],
        "/sync/push",
        &json!({
            "mutation_id":Uuid::new_v4().simple().to_string(),
            "table":"orders",
            "pk":order_id,
            "op":"upsert",
            "payload":{
                "status":"paid",
                "subtotal_cents":1590,
                "tax_cents":143,
                "shipping_cents":500,
                "total_cents":2233,
                "payment_ref":"demo-visa-4242",
                "items_json":items.to_string(),
                "created_at":created_at,
            }
        }),
    )
    .await?;
    for status in ["shipped", "delivered"] {
        invoke(
            client,
            endpoint,
            project,
            &accounts[0],
            "/sync/push",
            &json!({
                "mutation_id":Uuid::new_v4().simple().to_string(),
                "table":"orders",
                "pk":order_id,
                "op":"upsert",
                "payload":{"status":status},
            }),
        )
        .await?;
    }
    let customer = pull(client, endpoint, project, &accounts[1], after).await?;
    let other = pull(client, endpoint, project, &accounts[2], after).await?;
    let admin = pull(client, endpoint, project, &accounts[0], after).await?;
    for (label, view) in [("customer", &customer), ("admin", &admin)] {
        let changes = view["changes"]
            .as_array()
            .context("order pull omitted changes")?;
        let statuses: Vec<&str> = changes
            .iter()
            .filter(|change| {
                change["table"] == "order_events" && change["row"]["order_id"] == order_id
            })
            .filter_map(|change| change["row"]["status"].as_str())
            .collect();
        if statuses != ["paid", "shipped", "delivered"] {
            bail!("{label} did not receive ordered fulfilment events: {statuses:?}");
        }
        if !changes.iter().any(|change| {
            change["table"] == "orders"
                && change["pk"] == order_id
                && change["row"]["status"] == "delivered"
        }) {
            bail!("{label} did not receive delivered order");
        }
    }
    if has_pk(&other, &order_id)
        || other["changes"].as_array().is_some_and(|changes| {
            changes
                .iter()
                .any(|change| change["row"]["order_id"] == order_id)
        })
    {
        bail!("another customer saw the order or fulfilment events");
    }
    println!("Appwrite order workflow passed; retained demo order {order_id}");
    Ok(())
}

async fn pull(
    client: &reqwest::Client,
    endpoint: &str,
    project: &str,
    account: &Account,
    after: &str,
) -> Result<Value> {
    invoke(
        client,
        endpoint,
        project,
        account,
        "/sync/pull",
        &json!({"after":after,"limit":100}),
    )
    .await
}

async fn invoke(
    client: &reqwest::Client,
    endpoint: &str,
    project: &str,
    account: &Account,
    path: &str,
    body: &Value,
) -> Result<Value> {
    let response = client
        .post(format!("{endpoint}/functions/atlet_sync/executions"))
        .header("X-Appwrite-Project", project)
        .header("X-Appwrite-JWT", &account.jwt)
        .json(&json!({"body":body.to_string(),"method":"POST","path":path}))
        .send()
        .await?;
    let status = response.status();
    let execution: Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "Function execution HTTP {status}: {}",
            execution["message"].as_str().unwrap_or("unknown")
        );
    }
    let app_status = execution["responseStatusCode"].as_u64().unwrap_or(0);
    let output: Value = serde_json::from_str(execution["responseBody"].as_str().unwrap_or("null"))?;
    if !(200..300).contains(&app_status) {
        bail!(
            "Function {} returned {app_status}: {}",
            account.role,
            output
        );
    }
    Ok(output)
}

fn has_pk(response: &Value, pk: &str) -> bool {
    response["changes"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row["pk"] == pk))
}
