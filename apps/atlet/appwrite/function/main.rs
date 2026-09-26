//! Hosted Appwrite transport for the Atlet Nostos direct mode.

mod api;

use std::{collections::HashMap, env, thread, time::Duration};

use anyhow::{bail, Context as _, Result};
use api::Api;
use openruntimes::{Context, Response};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const DATABASE: &str = "atlet";
const MAX_PULL_ROWS: usize = 100;
const MAX_PUSH_ATTEMPTS: usize = 8;

#[derive(Debug)]
struct Fault {
    status: u16,
    message: &'static str,
}

impl Fault {
    fn bad(message: &'static str) -> Self {
        Self {
            status: 400,
            message,
        }
    }

    fn forbidden() -> Self {
        Self {
            status: 403,
            message: "forbidden",
        }
    }
}

#[derive(Clone)]
struct Principal {
    id: String,
    admin: bool,
}

#[derive(Deserialize)]
struct PullRequest {
    after: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    MAX_PULL_ROWS
}

#[derive(Deserialize)]
struct PushRequest {
    mutation_id: String,
    table: String,
    pk: String,
    op: String,
    payload: Option<Value>,
}

/// Appwrite's Rust runtime calls this exported entrypoint for each execution.
pub fn main(mut context: Context) -> Response {
    if context.req.method == "GET" && context.req.path == "/health" {
        return context
            .res
            .json(json!({"ok": true, "protocol": 1}), None, None);
    }
    let result = handle(&mut context);
    match result {
        Ok(value) => context.res.json(value, None, None),
        Err(error) => {
            if let Some(fault) = error.downcast_ref::<Fault>() {
                context
                    .res
                    .json(json!({"error": fault.message}), Some(fault.status), None)
            } else {
                context.error(format!("Atlet sync failure: {error:#}"));
                context.res.json(
                    json!({"error": "sync service unavailable"}),
                    Some(502),
                    None,
                )
            }
        }
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Fault {}

// This standalone Function cannot depend on nostos-infra's environment wrapper.
#[allow(clippy::disallowed_methods)]
fn handle(context: &mut Context) -> Result<Value> {
    if context.req.method != "POST" {
        return Err(Fault::bad("POST required").into());
    }
    let endpoint = env::var("APPWRITE_FUNCTION_API_ENDPOINT")?;
    let project = env::var("APPWRITE_FUNCTION_PROJECT_ID")?;
    let key = header(&context.req.headers, "x-appwrite-key")
        .context("Function dynamic API key missing")?;
    let jwt = header(&context.req.headers, "x-appwrite-user-jwt").ok_or(Fault {
        status: 401,
        message: "sign in required",
    })?;
    let api = Api::new(endpoint, project, key.to_string())?;
    let principal = authenticate(&api, jwt)?;
    match context.req.path.as_str() {
        "/sync/pull" => {
            let body: PullRequest = context
                .req
                .body_json()
                .map_err(|_| Fault::bad("invalid pull body"))?;
            pull(&api, &principal, body)
        }
        "/sync/push" => {
            let body: PushRequest = context
                .req
                .body_json()
                .map_err(|_| Fault::bad("invalid push body"))?;
            push(&api, &principal, body)
        }
        _ => Err(Fault {
            status: 404,
            message: "route not found",
        }
        .into()),
    }
}

fn header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn authenticate(api: &Api, jwt: &str) -> Result<Principal> {
    let account = api
        .get_with_jwt("account", jwt)
        .map_err(|error| {
            if error.to_string().contains("HTTP 401") {
                anyhow::Error::new(Fault {
                    status: 401,
                    message: "invalid session",
                })
            } else {
                error
            }
        })?
        .ok_or(Fault {
            status: 401,
            message: "invalid session",
        })?;
    let id = account["$id"]
        .as_str()
        .ok_or(Fault {
            status: 401,
            message: "invalid session",
        })?
        .to_string();
    let teams = api
        .get_with_jwt("teams", jwt)
        .map_err(|error| {
            if error.to_string().contains("HTTP 401") {
                anyhow::Error::new(Fault {
                    status: 401,
                    message: "invalid session",
                })
            } else {
                error
            }
        })?
        .context("team lookup failed")?;
    let admin = teams["teams"]
        .as_array()
        .is_some_and(|items| items.iter().any(|team| team["$id"] == "atlet_admins"));
    if let Some(profile) = api.get(&format!(
        "tablesdb/{DATABASE}/tables/user_profiles/rows/{id}"
    ))? {
        if row_data(&profile)["active"] == false {
            return Err(Fault {
                status: 403,
                message: "account inactive",
            }
            .into());
        }
    }
    Ok(Principal { id, admin })
}

fn pull(api: &Api, principal: &Principal, request: PullRequest) -> Result<Value> {
    let after: u64 = request
        .after
        .parse()
        .map_err(|_| Fault::bad("invalid cursor"))?;
    if request.limit == 0 || request.limit > MAX_PULL_ROWS {
        return Err(Fault::bad("limit must be 1..100").into());
    }
    let head = clock(api)?;
    if after > head {
        return Err(Fault::bad("cursor exceeds cloud head").into());
    }
    if after == head {
        return Ok(
            json!({"head": head.to_string(), "scanned_through": head.to_string(), "changes": [], "has_more": false, "principal_id":principal.id, "access_scope":if principal.admin {"admin"} else {"customer"}}),
        );
    }
    let queries = vec![
        json!({"method":"greaterThan","attribute":"seq","values":[after]}).to_string(),
        json!({"method":"lessThanEqual","attribute":"seq","values":[head]}).to_string(),
        json!({"method":"orderAsc","attribute":"seq"}).to_string(),
        json!({"method":"limit","values":[request.limit]}).to_string(),
    ];
    let list = api.list(
        &format!("tablesdb/{DATABASE}/tables/sync_changes/rows"),
        &queries,
    )?;
    let rows = list["rows"]
        .as_array()
        .context("journal list omitted rows")?;
    let mut scanned = after;
    let mut changes = Vec::new();
    for row in rows {
        let data = row_data(row);
        let seq = data["seq"]
            .as_u64()
            .context("journal row omitted sequence")?;
        if seq != scanned + 1 {
            bail!("journal gap at {}", scanned + 1);
        }
        scanned = seq;
        if visible(data, principal) {
            let payload = data["row_json"]
                .as_str()
                .map(serde_json::from_str::<Value>)
                .transpose()?;
            let op = if data["op"] == "upsert" {
                "insert"
            } else {
                data["op"]
                    .as_str()
                    .context("journal row omitted operation")?
            };
            changes.push(json!({
                "seq": seq.to_string(),
                "table": data["table_name"],
                "pk": data["pk"],
                "op": op,
                "row": payload,
            }));
        }
    }
    if scanned == after {
        bail!("journal empty before committed head {head}");
    }
    if rows.len() < request.limit && scanned < head {
        bail!("journal stopped at {scanned} before committed head {head}");
    }
    Ok(json!({
        "head": head.to_string(),
        "scanned_through": scanned.to_string(),
        "changes": changes,
        "has_more": scanned < head,
        "principal_id": principal.id,
        "access_scope": if principal.admin {"admin"} else {"customer"},
    }))
}

fn visible(change: &Value, principal: &Principal) -> bool {
    match change["audience"].as_str() {
        Some("public") => true,
        Some("owner") => change["owner_id"] == principal.id,
        Some("owner_admin") => principal.admin || change["owner_id"] == principal.id,
        _ => false,
    }
}

fn push(api: &Api, principal: &Principal, request: PushRequest) -> Result<Value> {
    if !valid_mutation_id(&request.mutation_id) {
        return Err(Fault::bad("invalid mutation ID").into());
    }
    if !valid_id(&request.pk) {
        return Err(Fault::bad("invalid row ID").into());
    }
    if !matches!(request.op.as_str(), "upsert" | "delete") {
        return Err(Fault::bad("unsupported write operation").into());
    }
    let audience = audience(&request.table).ok_or(Fault::bad("table is not writable"))?;
    let body_hash = hex::encode(Sha256::digest(
        json!({"actor":principal.id,"table":request.table,"pk":request.pk,"op":request.op,"payload":request.payload}).to_string().as_bytes(),
    ));
    for attempt in 0..MAX_PUSH_ATTEMPTS {
        if let Some(ledger) = api.get(&format!(
            "tablesdb/{DATABASE}/tables/sync_mutations/rows/{}",
            request.mutation_id
        ))? {
            let data = row_data(&ledger);
            if data["actor_id"] != principal.id || data["request_hash"] != body_hash {
                return Err(Fault::bad("mutation ID reused with different content").into());
            }
            let seq = data["seq"]
                .as_u64()
                .context("mutation ledger omitted sequence")?;
            return Ok(json!({"seq":seq.to_string(),"duplicate":true}));
        }
        let existing = api.get(&format!(
            "tablesdb/{DATABASE}/tables/{}/rows/{}",
            request.table, request.pk
        ))?;
        let (payload, owner) = authorize_write(api, principal, &request, existing.as_ref())?;
        let next = clock(api)? + 1;
        let order_event =
            order_event_payload(&request, existing.as_ref(), payload.as_ref(), &owner);
        let tx = api.post("tablesdb/transactions", &json!({"ttl":60}))?;
        let tx_id = tx["$id"].as_str().context("transaction omitted ID")?;
        let result = stage_write(StageWrite {
            api,
            tx_id,
            seq: next,
            request: &request,
            payload: payload.as_ref(),
            owner: &owner,
            audience,
            request_hash: &body_hash,
            actor: &principal.id,
            existed: existing.is_some(),
            order_event: order_event.as_ref(),
        });
        match result {
            Ok(()) => {
                return Ok(
                    json!({"seq":(next + u64::from(order_event.is_some())).to_string(),"duplicate":false}),
                )
            }
            Err(error) if error.to_string().contains("HTTP 409") => {
                let _ = api.patch(
                    &format!("tablesdb/transactions/{tx_id}"),
                    &json!({"rollback":true}),
                );
                thread::sleep(Duration::from_millis(30 * (attempt as u64 + 1)));
            }
            Err(error) => {
                let _ = api.patch(
                    &format!("tablesdb/transactions/{tx_id}"),
                    &json!({"rollback":true}),
                );
                return Err(error);
            }
        }
    }
    bail!("Appwrite clock conflict did not settle")
}

fn audience(table: &str) -> Option<&'static str> {
    match table {
        "products" | "attachments" => Some("public"),
        "sessions" | "cart_items" => Some("owner"),
        "orders" | "order_events" | "user_profiles" => Some("owner_admin"),
        _ => None,
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 36
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && id.as_bytes()[0].is_ascii_alphanumeric()
}

fn valid_mutation_id(id: &str) -> bool {
    match id.len() {
        32 => id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        36 => id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        }),
        _ => false,
    }
}

fn authorize_write(
    api: &Api,
    principal: &Principal,
    request: &PushRequest,
    existing: Option<&Value>,
) -> Result<(Option<Value>, String)> {
    let old = existing.map(row_data);
    let old_owner = old.and_then(|row| row["user_id"].as_str());
    if let Some(owner) = old_owner {
        if owner != principal.id
            && (!principal.admin || matches!(request.table.as_str(), "sessions" | "cart_items"))
        {
            return Err(Fault::forbidden().into());
        }
    }
    match request.table.as_str() {
        "products" | "attachments" if !principal.admin => return Err(Fault::forbidden().into()),
        "order_events" => return Err(Fault::forbidden().into()),
        "orders" if old.is_some() && !principal.admin => return Err(Fault::forbidden().into()),
        "orders" if request.op == "delete" => return Err(Fault::forbidden().into()),
        "user_profiles" if request.pk != principal.id && !principal.admin => {
            return Err(Fault::forbidden().into())
        }
        _ => {}
    }
    if request.op == "delete" {
        return Ok((None, old_owner.unwrap_or(&principal.id).to_string()));
    }
    let incoming = request
        .payload
        .as_ref()
        .and_then(Value::as_object)
        .ok_or(Fault::bad("upsert requires a JSON object"))?;
    let mut data: Map<String, Value> = old.and_then(Value::as_object).cloned().unwrap_or_default();
    data.extend(incoming.clone());
    data.remove("id");
    data.remove("$id");
    if request.table == "order_events" {
        let order_id = data
            .get("order_id")
            .and_then(Value::as_str)
            .ok_or(Fault::bad("order event needs order_id"))?;
        if !valid_id(order_id) {
            return Err(Fault::bad("invalid order ID").into());
        }
        let order = api
            .get(&format!(
                "tablesdb/{DATABASE}/tables/orders/rows/{order_id}"
            ))?
            .ok_or(Fault::bad("order not found"))?;
        let order_owner = row_data(&order)["user_id"]
            .as_str()
            .context("order has no owner")?;
        data.insert("user_id".to_string(), json!(order_owner));
    }
    if matches!(request.table.as_str(), "sessions" | "cart_items" | "orders") {
        let owner = old_owner.unwrap_or(&principal.id);
        if let Some(submitted) = data.get("user_id").and_then(Value::as_str) {
            if submitted != owner {
                return Err(Fault::forbidden().into());
            }
        }
        data.insert("user_id".to_string(), json!(owner));
    }
    if request.table == "sessions" {
        data.insert("server_committed_at".to_string(), json!(now()));
        if let Some(day) = data.get("occurred_on").and_then(Value::as_str) {
            if day.len() == 10 {
                data.insert("occurred_on".to_string(), json!(format!("{day}T00:00:00Z")));
            }
        }
    }
    if request.table == "cart_items" && data["qty"].as_i64().unwrap_or(0) <= 0 {
        return Err(Fault::bad("cart quantity must be positive").into());
    }
    if request.table == "user_profiles" {
        data.insert("user_id".to_string(), json!(request.pk));
        if !principal.admin {
            data.insert(
                "active".to_string(),
                old.and_then(|row| row.get("active"))
                    .cloned()
                    .unwrap_or(json!(true)),
            );
        }
        if request.pk == principal.id && data.get("active") == Some(&json!(false)) {
            return Err(Fault::bad("cannot disable your own sync access").into());
        }
    }
    if request.table == "orders" && old.is_none() {
        let created_at = data
            .get("created_at")
            .and_then(Value::as_str)
            .ok_or(Fault::bad("invalid order timestamp"))?;
        let created_at = chrono::DateTime::parse_from_rfc3339(created_at)
            .map_err(|_| Fault::bad("invalid order timestamp"))?
            .to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        data.insert("created_at".to_string(), json!(created_at));
        if data.get("status") != Some(&json!("paid"))
            || data.get("payment_ref") != Some(&json!("demo-visa-4242"))
        {
            return Err(Fault::bad("demo order must start paid").into());
        }
        let subtotal = data["subtotal_cents"]
            .as_i64()
            .ok_or(Fault::bad("invalid subtotal"))?;
        let tax = data["tax_cents"]
            .as_i64()
            .ok_or(Fault::bad("invalid tax"))?;
        let shipping = data["shipping_cents"]
            .as_i64()
            .ok_or(Fault::bad("invalid shipping"))?;
        let total = data["total_cents"]
            .as_i64()
            .ok_or(Fault::bad("invalid total"))?;
        if subtotal < 0 || tax < 0 || shipping < 0 || total != subtotal + tax + shipping {
            return Err(Fault::bad("invalid order totals").into());
        }
        let lines: Value = serde_json::from_str(
            data["items_json"]
                .as_str()
                .ok_or(Fault::bad("order items missing"))?,
        )
        .map_err(|_| Fault::bad("invalid order items"))?;
        let lines = lines
            .as_array()
            .ok_or(Fault::bad("order items must be an array"))?;
        if lines.is_empty() {
            return Err(Fault::bad("order must contain items").into());
        }
        let mut calculated = 0i64;
        for line in lines {
            let qty = line["qty"]
                .as_i64()
                .ok_or(Fault::bad("invalid item quantity"))?;
            let price = line["price_cents"]
                .as_i64()
                .ok_or(Fault::bad("invalid item price"))?;
            if qty <= 0 || price < 0 {
                return Err(Fault::bad("invalid order item").into());
            }
            calculated = calculated
                .checked_add(qty.checked_mul(price).ok_or(Fault::bad("order overflow"))?)
                .ok_or(Fault::bad("order overflow"))?;
        }
        let expected_tax = calculated
            .checked_mul(9)
            .and_then(|n| n.checked_add(50))
            .ok_or(Fault::bad("order overflow"))?
            / 100;
        let expected_shipping = if calculated >= 5_000 { 0 } else { 500 };
        if subtotal != calculated || tax != expected_tax || shipping != expected_shipping {
            return Err(Fault::bad("order pricing mismatch").into());
        }
    }
    if request.table == "orders" {
        if let Some(previous) = old {
            let from = previous["status"].as_str().unwrap_or_default();
            let to = data["status"].as_str().unwrap_or_default();
            if !matches!((from, to), ("paid", "shipped") | ("shipped", "delivered")) {
                return Err(Fault::bad("invalid order status transition").into());
            }
            for field in [
                "user_id",
                "subtotal_cents",
                "tax_cents",
                "shipping_cents",
                "total_cents",
                "payment_ref",
                "items_json",
                "created_at",
            ] {
                let matches = match (previous.get(field), data.get(field)) {
                    (Some(stored), Some(incoming)) => {
                        immutable_order_field_matches(field, stored, incoming)
                    }
                    (None, None) => true,
                    _ => false,
                };
                if !matches {
                    return Err(Fault::bad("order details are immutable").into());
                }
            }
            // TablesDB serializes UTC dates with +00:00; Flutter writes the
            // same instant with Z. Preserve the stored representation.
            if let Some(created_at) = previous.get("created_at") {
                data.insert("created_at".to_string(), created_at.clone());
            }
        }
    }
    let owner = if request.table == "user_profiles" {
        request.pk.clone()
    } else {
        data.get("user_id")
            .and_then(Value::as_str)
            .unwrap_or(&principal.id)
            .to_string()
    };
    Ok((Some(Value::Object(data)), owner))
}

fn immutable_order_field_matches(field: &str, stored: &Value, incoming: &Value) -> bool {
    if field != "created_at" {
        return stored == incoming;
    }
    match (stored.as_str(), incoming.as_str()) {
        (Some(stored), Some(incoming)) => {
            match (
                chrono::DateTime::parse_from_rfc3339(stored),
                chrono::DateTime::parse_from_rfc3339(incoming),
            ) {
                (Ok(stored), Ok(incoming)) => {
                    stored.timestamp_millis() == incoming.timestamp_millis()
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn order_event_payload(
    request: &PushRequest,
    existing: Option<&Value>,
    payload: Option<&Value>,
    owner: &str,
) -> Option<Value> {
    if request.table != "orders" {
        return None;
    }
    let status = payload?.get("status")?.as_str()?;
    let previous = existing.and_then(|row| row_data(row)["status"].as_str());
    if previous == Some(status) {
        return None;
    }
    Some(json!({
        "user_id":owner,
        "order_id":request.pk,
        "status":status,
        "previous_status":previous,
        "created_at":now(),
    }))
}

struct StageWrite<'a> {
    api: &'a Api,
    tx_id: &'a str,
    seq: u64,
    request: &'a PushRequest,
    payload: Option<&'a Value>,
    owner: &'a str,
    audience: &'a str,
    request_hash: &'a str,
    actor: &'a str,
    existed: bool,
    order_event: Option<&'a Value>,
}

fn stage_write(input: StageWrite<'_>) -> Result<()> {
    let StageWrite {
        api,
        tx_id,
        seq,
        request,
        payload,
        owner,
        audience,
        request_hash,
        actor,
        existed,
        order_event,
    } = input;
    let terminal_seq = seq + u64::from(order_event.is_some());
    api.patch(
        &format!("tablesdb/{DATABASE}/tables/sync_clock/rows/clock"),
        &json!({"data":{"head":terminal_seq},"transactionId":tx_id}),
    )?;
    if let Some(payload) = payload {
        api.put(
            &format!(
                "tablesdb/{DATABASE}/tables/{}/rows/{}",
                request.table, request.pk
            ),
            &json!({"data":payload,"permissions":[],"transactionId":tx_id}),
        )?;
    } else if existed {
        api.delete(&format!(
            "tablesdb/{DATABASE}/tables/{}/rows/{}?transactionId={tx_id}",
            request.table, request.pk
        ))?;
    }
    let mut row_json = payload.cloned();
    if let Some(row) = row_json.as_mut() {
        row["id"] = json!(request.pk);
    }
    api.post(&format!("tablesdb/{DATABASE}/tables/sync_changes/rows"), &json!({
        "rowId":format!("c{seq:019}"),
        "data":{"seq":seq,"table_name":request.table,"pk":request.pk,"op":if payload.is_some(){if existed {"update"} else {"insert"}}else{"delete"},"row_json":row_json.map(|row|row.to_string()),"owner_id":owner,"audience":audience},
        "permissions":[],"transactionId":tx_id,
    }))?;
    if let Some(event) = order_event {
        let event_seq = seq + 1;
        let event_id = format!("e{event_seq:019}");
        api.post(
            &format!("tablesdb/{DATABASE}/tables/order_events/rows"),
            &json!({
                "rowId":event_id,
                "data":event,
                "permissions":[],
                "transactionId":tx_id,
            }),
        )?;
        let mut event_image = event.clone();
        event_image["id"] = json!(event_id);
        api.post(&format!("tablesdb/{DATABASE}/tables/sync_changes/rows"), &json!({
            "rowId":format!("c{event_seq:019}"),
            "data":{"seq":event_seq,"table_name":"order_events","pk":event_id,"op":"insert","row_json":event_image.to_string(),"owner_id":owner,"audience":"owner_admin"},
            "permissions":[],"transactionId":tx_id,
        }))?;
    }
    api.post(
        &format!("tablesdb/{DATABASE}/tables/sync_mutations/rows"),
        &json!({
            "rowId":request.mutation_id,
            "data":{"seq":terminal_seq,"actor_id":actor,"request_hash":request_hash,"created_at":now()},
            "permissions":[],"transactionId":tx_id,
        }),
    )?;
    api.patch(
        &format!("tablesdb/transactions/{tx_id}"),
        &json!({"commit":true}),
    )?;
    Ok(())
}

fn clock(api: &Api) -> Result<u64> {
    let row = api
        .get(&format!("tablesdb/{DATABASE}/tables/sync_clock/rows/clock"))?
        .context("sync clock missing")?;
    row_data(&row)["head"]
        .as_u64()
        .context("sync clock has no head")
}

fn row_data(row: &Value) -> &Value {
    row.get("data").unwrap_or(row)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_feed_entries_do_not_leak_to_other_users_or_admins() {
        let user = Principal {
            id: "alice".into(),
            admin: false,
        };
        let admin = Principal {
            id: "admin".into(),
            admin: true,
        };
        let private = json!({"audience":"owner","owner_id":"alice"});
        assert!(visible(&private, &user));
        assert!(!visible(&private, &admin));
        let order = json!({"audience":"owner_admin","owner_id":"bob"});
        assert!(!visible(&order, &user));
        assert!(visible(&order, &admin));
    }

    #[test]
    fn mutation_ids_accept_both_uuid_encodings() {
        assert!(valid_mutation_id("0123456789abcdef0123456789abcdef"));
        assert!(valid_mutation_id("01234567-89ab-cdef-0123-456789abcdef"));
        assert!(!valid_mutation_id("../other"));
    }

    #[test]
    fn order_transition_generates_one_owner_scoped_event() {
        let request = PushRequest {
            mutation_id: "0123456789abcdef0123456789abcdef".into(),
            table: "orders".into(),
            pk: "order-1".into(),
            op: "upsert".into(),
            payload: None,
        };
        let old = json!({"status":"paid"});
        let next = json!({"status":"shipped"});
        let event = order_event_payload(&request, Some(&old), Some(&next), "buyer")
            .expect("status transition needs event");
        assert_eq!(event["user_id"], "buyer");
        assert_eq!(event["previous_status"], "paid");
        assert_eq!(event["status"], "shipped");
        assert!(order_event_payload(&request, Some(&next), Some(&next), "buyer").is_none());
    }

    #[test]
    fn appwrite_and_flutter_timestamp_encodings_are_the_same_immutable_value() {
        let stored = json!("2026-09-26T06:27:31.873+00:00");
        let flutter = json!("2026-09-26T06:27:31.873Z");
        assert!(immutable_order_field_matches(
            "created_at",
            &stored,
            &flutter
        ));
        assert!(immutable_order_field_matches(
            "created_at",
            &stored,
            &json!("2026-09-26T06:27:31.873560Z"),
        ));
        assert!(!immutable_order_field_matches(
            "created_at",
            &stored,
            &json!("2026-09-26T06:27:32.873Z"),
        ));
    }
}
