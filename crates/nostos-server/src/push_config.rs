//! Push-tables config parsing (ADR-0037 §1 amendment + §2, plan 2.4) and the
//! push notifier wiring decision (ADR-0038 §3).

/// `^[a-z_][a-z0-9_]*$` — the ADR-0013 identifier shape, char-class edition
/// (the regex itself lives behind the write-back adapter; a config parser
/// doesn't need it).
fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('_' | 'a'..='z'))
        && chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `NOSTOS_PUSH_TABLES` parse output: the application-layer [`PushTables`]
/// plus the infra-side Live Activity content-state templates (plan task 6.4,
/// experimental). A `liveactivity` entry appears in BOTH maps: the
/// `PushTables` row is a placeholder `Visible` — the only variant the
/// application layer attaches tuple bytes for (`fanout.rs:337`) — and the
/// router consults `live_activities` FIRST, so the placeholder never
/// renders. ponytail: placeholder coupling; the upgrade is a real
/// `PushTemplate::LiveActivity` variant when the application crate accepts
/// new variants again.
#[derive(Debug, Default)]
pub(crate) struct PushTablesConfig {
    pub(crate) tables: nostos_application::ports::PushTables,
    pub(crate) live_activities: std::collections::HashMap<String, serde_json::Value>,
}

pub(crate) fn resolve_tenant_col<'a>(sync_auth: &str, tenant_column: &'a str) -> Option<&'a str> {
    if !tenant_column.is_empty() && (sync_auth == "supabase-jwt" || sync_auth == "bearer") {
        Some(tenant_column)
    } else {
        None
    }
}

/// The one well-known routing key an operator can configure: where a tap on
/// this table's notification should land. Anything richer goes through
/// nostos-pushd's `/v1/send` (whose `data` is an arbitrary map).
///
/// ponytail: one key, not a map, because `NOSTOS_PUSH_TABLES` is a
/// colon/semicolon string — a map means JSON-in-env. Promote it when a
/// second key is actually asked for.
const PUSH_ROUTE_KEY: &str = "cairn_route";

fn push_route_data(
    route: Option<&str>,
    entry: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut data = std::collections::BTreeMap::new();
    if let Some(route) = route {
        if !route.starts_with('/') {
            anyhow::bail!(
                "NOSTOS_PUSH_TABLES: route {route:?} must start with '/' \
                 (table:visible@/orders/{{id}}:<title>:<body>) in {entry:?}"
            );
        }
        if route.chars().any(char::is_whitespace) {
            anyhow::bail!("NOSTOS_PUSH_TABLES: route {route:?} must not contain whitespace");
        }
        data.insert(PUSH_ROUTE_KEY.to_string(), route.to_string());
    }
    nostos_infra::push::validate_data(&data)
        .map_err(|e| anyhow::anyhow!("NOSTOS_PUSH_TABLES: {e} in {entry:?}"))?;
    Ok(data)
}

pub(crate) fn parse_push_tables(
    raw: &str,
    tenant_column: Option<&str>,
) -> anyhow::Result<PushTablesConfig> {
    use nostos_application::ports::{PushTables, PushTemplate};

    let mut tables = std::collections::HashMap::new();
    let mut live_activities = std::collections::HashMap::new();
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(2, ':');
        let table = parts.next().unwrap_or_default().trim().to_string();
        if table.is_empty() {
            anyhow::bail!("NOSTOS_PUSH_TABLES: empty table name in entry {entry:?}");
        }
        if !is_plain_identifier(&table) {
            anyhow::bail!(
                "NOSTOS_PUSH_TABLES: table name {table:?} must match ^[a-z_][a-z0-9_]*$ (ADR-0013)"
            );
        }
        let template = match parts.next() {
            None => PushTemplate::Silent,
            Some(rest) => {
                // `visible[image=…,collapse=…]` — presentation options
                // (ADR-0047), split off before the colon split because
                // their values hold colons.
                let (rest, options) = nostos_infra::push::take_options(rest)
                    .map_err(|e| anyhow::anyhow!("NOSTOS_PUSH_TABLES: {e} in {entry:?}"))?;
                let rest = rest.as_str();
                let (mode, args) = match rest.split_once(':') {
                    Some((m, a)) => (m.trim(), Some(a)),
                    None => (rest.trim(), None),
                };
                // `visible@/orders/{id}` — the optional deep link a tap
                // resolves to (ADR-0037 §2 amendment). `@` rather than one
                // more `:` because the body is the greedy remainder: this
                // syntax has no free colon position left.
                let (mode, route) = match mode.split_once('@') {
                    Some((m, r)) => (m.trim(), Some(r.trim())),
                    None => (mode, None),
                };
                if route.is_some() && !matches!(mode, "visible" | "action") {
                    anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: only visible/action entries take an @route \
                         (a doorbell carries no routing keys): {entry:?}"
                    );
                }
                if !options.is_empty() && !matches!(mode, "visible" | "action") {
                    anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: only visible/action entries take [options]: {entry:?}"
                    );
                }
                let data = push_route_data(route, entry)?;
                match (mode, args) {
                    ("silent", None) => PushTemplate::Silent,
                    ("silent", Some(_)) => {
                        anyhow::bail!(
                            "NOSTOS_PUSH_TABLES: \"silent\" entries take no title/body: {entry:?}"
                        )
                    }
                    ("visible", Some(title_body)) => {
                        // Title runs to the next ':'; body keeps any further
                        // colons (the old `splitn(4)` remainder semantics).
                        match title_body.split_once(':') {
                            Some((title, body)) => PushTemplate::Visible {
                                title: title.trim().to_string(),
                                body: body.trim().to_string(),
                                category: None,
                                data,
                                options,
                            },
                            None => anyhow::bail!(
                                "NOSTOS_PUSH_TABLES: \"visible\" entries need a title and a body: \
                                 table:visible:<title>:<body> (got {entry:?})"
                            ),
                        }
                    }
                    ("visible", None) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: \"visible\" entries need a title and a body: \
                         table:visible:<title>:<body> (got {entry:?})"
                    ),
                    // Action push (ADR-0037 §2): a visible notification whose
                    // banner carries the client-registered `category`'s action
                    // buttons. Category BEFORE title keeps parsing unambiguous
                    // (body stays the greedy remainder and may contain colons).
                    // The category is the contract between the operator's rule
                    // and the app's registered `UNNotificationCategory` /
                    // Android local-notification actions — enforce identifier
                    // discipline so typos fail at startup, not in production.
                    ("action", Some(cat_title_body)) => match cat_title_body.split_once(':') {
                        Some((category, title_body)) => {
                            let category = category.trim();
                            if !is_plain_identifier(category) {
                                anyhow::bail!(
                                    "NOSTOS_PUSH_TABLES: action category {category:?} must \
                                         match ^[a-z_][a-z0-9_]*$ in {entry:?}"
                                );
                            }
                            match title_body.split_once(':') {
                                Some((title, body)) => PushTemplate::Visible {
                                    title: title.trim().to_string(),
                                    body: body.trim().to_string(),
                                    category: Some(category.to_string()),
                                    data,
                                    options,
                                },
                                None => anyhow::bail!(
                                    "NOSTOS_PUSH_TABLES: \"action\" entries need a category, \
                                         a title and a body: \
                                         table:action:<category>:<title>:<body> (got {entry:?})"
                                ),
                            }
                        }
                        None => anyhow::bail!(
                                "NOSTOS_PUSH_TABLES: \"action\" entries need a category, a title \
                                 and a body: table:action:<category>:<title>:<body> (got {entry:?})"
                            ),
                    },
                    ("action", None) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: \"action\" entries need a category, a title and a \
                         body: table:action:<category>:<title>:<body> (got {entry:?})"
                    ),
                    ("liveactivity", Some(tpl)) => {
                        let tpl = tpl.trim();
                        let value: serde_json::Value = serde_json::from_str(tpl).map_err(|e| {
                            anyhow::anyhow!(
                                "NOSTOS_PUSH_TABLES: liveactivity template for {table:?} is not \
                                 valid JSON: {e}"
                            )
                        })?;
                        if !value.is_object() {
                            anyhow::bail!(
                                "NOSTOS_PUSH_TABLES: liveactivity template for {table:?} must be \
                                 a JSON object (the ActivityKit content-state), got {tpl:?}"
                            );
                        }
                        live_activities.insert(table.clone(), value);
                        // Placeholder — see `PushTablesConfig`; the router's
                        // live_activities lookup shadows it before render.
                        PushTemplate::Visible {
                            title: String::new(),
                            body: String::new(),
                            category: None,
                            data,
                            options,
                        }
                    }
                    ("liveactivity", None) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: \"liveactivity\" entries need a JSON content-state \
                         template: table:liveactivity:{{\"col\":\"{{col}}\"}} (got {entry:?})"
                    ),
                    (other, _) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: unknown mode {other:?} in {entry:?} (expected \
                         silent, visible or liveactivity)"
                    ),
                }
            }
        };
        if tables.insert(table.clone(), template).is_some() {
            anyhow::bail!("NOSTOS_PUSH_TABLES: table {table:?} listed twice");
        }
    }
    Ok(PushTablesConfig {
        tables: PushTables {
            tenant_column: tenant_column.map(str::to_string),
            tables,
        },
        live_activities,
    })
}

/// Which push notifier the composition root wires — the ADR-0038 §3
/// precedence as a pure decision (plan task 2.3):
///
/// 1. `NOSTOS_PUSH_REMOTE_URL` + `NOSTOS_PUSH_REMOTE_KEY` BOTH set ⇒
///    [`PushWiring::Remote`] (delegation; the embedded router is skipped
///    entirely — rails live daemon-side).
/// 2. Exactly one of the two set ⇒ config error (refuse to start).
/// 3. Both unset ⇒ [`PushWiring::Embedded`] when anything can deliver
///    (`embedded_armed`: a rail configured or push tables listed),
///    [`PushWiring::Noop`] otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushWiring {
    Remote,
    Embedded,
    Noop,
}

pub(crate) fn push_wiring(
    remote_url: &str,
    remote_key: &str,
    embedded_armed: bool,
) -> Result<PushWiring, String> {
    let url = !remote_url.trim().is_empty();
    let key = !remote_key.trim().is_empty();
    match (url, key) {
        (true, true) => Ok(PushWiring::Remote),
        (false, false) => Ok(if embedded_armed {
            PushWiring::Embedded
        } else {
            PushWiring::Noop
        }),
        _ => Err("push delegation requires BOTH NOSTOS_PUSH_REMOTE_URL and \
             NOSTOS_PUSH_REMOTE_KEY (exactly one is set)"
            .to_string()),
    }
}

#[cfg(test)]
mod push_wiring_tests {
    use super::{push_wiring, PushWiring};

    #[test]
    fn both_remote_vars_set_wins_over_everything() {
        // Delegation beats the embedded path even with rails+tables armed:
        // rails live daemon-side when delegating.
        assert_eq!(
            push_wiring("http://127.0.0.1:8090", "tenant-secret", true),
            Ok(PushWiring::Remote)
        );
        assert_eq!(
            push_wiring("http://127.0.0.1:8090", "tenant-secret", false),
            Ok(PushWiring::Remote)
        );
    }

    #[test]
    fn exactly_one_remote_var_is_a_config_error() {
        assert!(push_wiring("http://127.0.0.1:8090", "", true).is_err());
        assert!(push_wiring("", "tenant-secret", false).is_err());
        // Blank-but-set counts as unset (same rule as the rail envs).
        assert!(push_wiring("   ", "", false).is_ok());
    }

    #[test]
    fn unset_falls_back_to_embedded_or_noop() {
        assert_eq!(push_wiring("", "", true), Ok(PushWiring::Embedded));
        assert_eq!(push_wiring("", "", false), Ok(PushWiring::Noop));
    }
}

#[cfg(test)]
#[cfg(test)]
mod resolve_tenant_col_tests {
    use super::resolve_tenant_col;

    #[test]
    fn supabase_jwt_with_column_scopes() {
        assert_eq!(resolve_tenant_col("supabase-jwt", "org_id"), Some("org_id"));
    }

    #[test]
    fn bearer_with_column_scopes_to_the_fixed_principal_tenant() {
        assert_eq!(resolve_tenant_col("bearer", "org_id"), Some("org_id"));
    }

    #[test]
    fn none_auth_never_scopes() {
        assert_eq!(resolve_tenant_col("none", "org_id"), None);
    }

    #[test]
    fn empty_column_is_the_explicit_opt_out_in_every_mode() {
        assert_eq!(resolve_tenant_col("supabase-jwt", ""), None);
        assert_eq!(resolve_tenant_col("bearer", ""), None);
        assert_eq!(resolve_tenant_col("none", ""), None);
    }
}

#[cfg(test)]
mod parse_push_tables_tests {
    use super::{is_plain_identifier, parse_push_tables};
    use nostos_application::ports::PushTemplate;
    use serde_json::json;

    #[test]
    fn parses_action_mode_with_category_and_rejects_bad_categories() {
        let cfg = parse_push_tables(
            "orders:action:order_status:Atlet order update:Your order {id} is {status}:really",
            None,
        )
        .expect("valid action config");
        assert_eq!(
            cfg.tables.get("orders"),
            Some(&PushTemplate::Visible {
                title: "Atlet order update".into(),
                // Body keeps further colons (greedy remainder semantics).
                body: "Your order {id} is {status}:really".into(),
                category: Some("order_status".into()),
                data: std::collections::BTreeMap::new(),
                options: std::collections::BTreeMap::new(),
            })
        );

        let err = parse_push_tables("orders:action:Order Status:t:b", None);
        assert!(err.is_err(), "category must be a plain identifier");

        let err = parse_push_tables("orders:action:order_status:only-title", None);
        assert!(err.is_err(), "action entries need category, title AND body");

        let err = parse_push_tables("orders:action", None);
        assert!(err.is_err(), "bare action mode is rejected");
    }

    #[test]
    fn parses_an_options_group_after_mode_and_route() {
        let cfg = parse_push_tables(
            "order_events:action@/history/{id}[image=https://cdn.example/{status}.png, \
             collapse=order-{order_id}]:order_status:{icon} Update:Order is {status}",
            None,
        )
        .expect("valid options group");
        let Some(PushTemplate::Visible {
            title,
            category,
            data,
            options,
            ..
        }) = cfg.tables.get("order_events")
        else {
            panic!("order_events must be a visible template");
        };
        assert_eq!(title, "{icon} Update");
        assert_eq!(category.as_deref(), Some("order_status"));
        assert_eq!(
            data.get("cairn_route").map(String::as_str),
            Some("/history/{id}")
        );
        assert_eq!(
            options.get("image").map(String::as_str),
            Some("https://cdn.example/{status}.png")
        );
        assert_eq!(
            options.get("collapse").map(String::as_str),
            Some("order-{order_id}")
        );

        for bad in [
            "orders:silent[image=https://x.png]",
            "orders:visible[level=loud]:t:b",
            "orders:visible[image=https://x.png:t:b",
        ] {
            assert!(
                parse_push_tables(bad, None).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn parses_route_suffix_into_a_routing_key() {
        let cfg = parse_push_tables(
            "orders:visible@/orders/{id}:New order:Order {id} placed;\
             deliveries:action@/deliveries/{id}:order_status:Out for delivery:{id}",
            None,
        )
        .expect("valid @route config");
        assert_eq!(
            cfg.tables.get("orders"),
            Some(&PushTemplate::Visible {
                title: "New order".into(),
                body: "Order {id} placed".into(),
                category: None,
                // `{id}` stays a placeholder here — the router interpolates
                // it against the row that actually committed.
                data: [("cairn_route".to_string(), "/orders/{id}".to_string())]
                    .into_iter()
                    .collect(),
                options: std::collections::BTreeMap::new(),
            })
        );
        assert_eq!(
            cfg.tables.get("deliveries").and_then(|t| match t {
                PushTemplate::Visible { data, .. } => data.get("cairn_route"),
                PushTemplate::Silent => None,
            }),
            Some(&"/deliveries/{id}".to_string()),
            "action entries route too"
        );

        for bad in [
            // A doorbell has nothing to route.
            "orders:silent@/orders/{id}",
            "orders@/orders/{id}",
            // Relative routes and whitespace fail at startup, not in prod.
            "orders:visible@orders/{id}:t:b",
            "orders:visible@/orders/{id} x:t:b",
        ] {
            assert!(parse_push_tables(bad, None).is_err(), "{bad:?} must fail");
        }
    }

    #[test]
    fn parses_silent_default_explicit_and_visible_with_placeholders() {
        let cfg = parse_push_tables(
            "tasks; notes:silent ; orders:visible:New order:Order {id} placed",
            Some("org_id"),
        )
        .expect("valid config");
        assert_eq!(cfg.tables.tenant_column.as_deref(), Some("org_id"));
        assert_eq!(cfg.tables.get("tasks"), Some(&PushTemplate::Silent));
        assert_eq!(cfg.tables.get("notes"), Some(&PushTemplate::Silent));
        assert_eq!(
            cfg.tables.get("orders"),
            Some(&PushTemplate::Visible {
                title: "New order".into(),
                body: "Order {id} placed".into(),
                category: None,
                data: std::collections::BTreeMap::new(),
                options: std::collections::BTreeMap::new(),
            })
        );
        assert_eq!(cfg.tables.get("absent"), None);
        assert!(cfg.live_activities.is_empty());
    }

    #[test]
    fn empty_string_is_an_empty_config() {
        let cfg = parse_push_tables("", None).expect("empty is valid (push off)");
        assert!(cfg.tables.tables.is_empty());
        assert!(cfg.tables.tenant_column.is_none());
        assert!(cfg.live_activities.is_empty());
    }

    #[test]
    fn rejects_bad_modes_missing_body_bad_identifiers_and_duplicates() {
        for bad in [
            "tasks:loud",
            "orders:visible:OnlyTitle",
            "Orders:visible:a:b",
            "tasks;tasks",
            "tasks:silent:extra",
        ] {
            assert!(
                parse_push_tables(bad, None).is_err(),
                "{bad:?} must be rejected at startup"
            );
        }
    }

    #[test]
    fn liveactivity_entry_parses_template_and_placeholders() {
        let cfg = parse_push_tables(
            r#"deliveries:liveactivity:{"status":"{status}","eta_min":"{eta_min}","nested":{"deep":"{x}"}}"#,
            None,
        )
        .expect("valid liveactivity config");
        // The tables map carries the Visible placeholder so fan-out attaches
        // tuple bytes (see PushTablesConfig); the real template is separate.
        assert_eq!(
            cfg.tables.get("deliveries"),
            Some(&PushTemplate::Visible {
                title: String::new(),
                body: String::new(),
                category: None,
                data: std::collections::BTreeMap::new(),
                options: std::collections::BTreeMap::new(),
            })
        );
        assert_eq!(
            cfg.live_activities.get("deliveries"),
            Some(
                &json!({ "status": "{status}", "eta_min": "{eta_min}", "nested": { "deep": "{x}" } })
            )
        );
    }

    #[test]
    fn liveactivity_entries_must_be_a_json_object() {
        for bad in [
            r"deliveries:liveactivity:not json",
            r"deliveries:liveactivity:[1,2]",
            r#"deliveries:liveactivity:"string""#,
            "deliveries:liveactivity",
        ] {
            assert!(
                parse_push_tables(bad, None).is_err(),
                "{bad:?} must be rejected at startup"
            );
        }
    }

    #[test]
    fn identifier_shape_matches_adr0013() {
        for good in ["tasks", "a", "_x", "t1_2"] {
            assert!(is_plain_identifier(good), "{good} should pass");
        }
        for bad in ["Tasks", "1t", "a-b", "", "a b"] {
            assert!(!is_plain_identifier(bad), "{bad} should fail");
        }
    }
}

/// ADR-0037 "the test that matters" — server-side slice (plan 3.3): the real
/// `FanOutService` hint enqueue → the real `PushRouter` coalescer, against a
/// recording fake rail, the in-memory token registry, and the REAL
/// `InMemorySessionStore` (presence = store membership, so a `Dropped`-but-
/// registered session counts as online). The fake replicator's payload is
/// opaque, so the extractor hands the tenant column out directly.
#[cfg(test)]
mod push_e2e_tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use nostos_application::ports::{
        DeliveryDecision, EventSink, Metrics, PushNotifier, PushTables, PushTemplate, SyncAuth,
    };
    use nostos_application::FanOutService;
    use nostos_domain::{
        ColumnValue, Lsn, Predicate, Principal, ReplicationEvent, RowOp, SyncSession,
    };
    use nostos_infra::push::{PushPayload, RailOutcome};
    use nostos_infra::{
        InMemorySessionStore, InMemoryTokenRegistry, PushRouter, PushSink, PushTokenRegistry,
    };

    use crate::push_api::{self, PushApiState};

    /// The fake rail: records every send, always reports Delivered.
    struct RecordingRail {
        sends: Mutex<Vec<(String, String, PushPayload)>>, // (platform, token, payload)
        live_sends: Mutex<Vec<(String, String, serde_json::Value)>>, // (token, collapse, state)
    }

    #[async_trait]
    impl PushSink for RecordingRail {
        async fn send(
            &self,
            platform: &str,
            token: &str,
            _collapse_key: &str,
            payload: &PushPayload,
        ) -> RailOutcome {
            self.sends.lock().unwrap().push((
                platform.to_string(),
                token.to_string(),
                payload.clone(),
            ));
            RailOutcome::Delivered
        }

        async fn send_live_activity(
            &self,
            token: &str,
            _collapse_key: &str,
            content_state: &serde_json::Value,
        ) -> RailOutcome {
            self.live_sends.lock().unwrap().push((
                token.to_string(),
                _collapse_key.to_string(),
                content_state.clone(),
            ));
            RailOutcome::Delivered
        }
    }

    /// A slow-client sink: always `Dropped`, still a live session.
    struct DroppingSink;

    #[async_trait]
    impl EventSink for DroppingSink {
        async fn deliver(&self, _event: Arc<ReplicationEvent>) -> DeliveryDecision {
            DeliveryDecision::Dropped
        }
    }

    /// Always resolves to one fixed principal — the authenticated test path.
    struct FixedAuth(Principal);

    #[async_trait]
    impl SyncAuth for FixedAuth {
        async fn authenticate(&self, _token: &str) -> Option<Principal> {
            Some(self.0.clone())
        }
    }

    fn push_tables() -> PushTables {
        PushTables {
            tenant_column: Some("org_id".into()),
            tables: [("tasks".to_string(), PushTemplate::Silent)]
                .into_iter()
                .collect(),
        }
    }

    fn event(lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: "tasks".into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        )
    }

    /// The tenant column extractor: `org_id` → t1 (the fake payload is
    /// opaque bytes, so the value is handed out directly).
    fn extract(_e: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
        (col == "org_id").then(|| ColumnValue::text("t1"))
    }

    /// Build the full chain: store + registry + rail + router + fan-out.
    /// `session` optionally registers a live session for account u1 first.
    async fn harness(
        session: Option<Arc<dyn EventSink>>,
    ) -> (
        Arc<RecordingRail>,
        Arc<InMemoryTokenRegistry>,
        Arc<FanOutService>,
    ) {
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        if let Some(sink) = session {
            store
                .add(
                    SyncSession::new_authenticated(
                        Predicate::all("tasks"),
                        Principal::new("u1", "t1"),
                    ),
                    sink,
                )
                .await;
        }
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "dev-e2e", "u1", "t1")
            .await
            .unwrap();
        let rail = Arc::new(RecordingRail {
            sends: Mutex::new(Vec::new()),
            live_sends: Mutex::new(Vec::new()),
        });
        let registry_dyn: Arc<dyn PushTokenRegistry> = registry.clone();
        let router = PushRouter::new(
            Arc::clone(&rail) as Arc<dyn PushSink>,
            registry_dyn,
            Arc::clone(&store),
            nostos_infra::push::router::RouterConfig {
                tables: push_tables(),
                live_activities: std::collections::HashMap::new(),
            },
            Duration::from_millis(60),
            Arc::new(Metrics::new()),
        );
        let svc = Arc::new(
            FanOutService::new(Arc::clone(&store))
                .with_push_tables(push_tables())
                .with_push_notifier(Arc::new(router) as Arc<dyn PushNotifier>),
        );
        (rail, registry, svc)
    }

    async fn burst(svc: &FanOutService) {
        for lsn in 1..=100u64 {
            let _ = svc.fan_out(&event(lsn), extract).await;
        }
    }

    async fn soon(mut f: impl FnMut() -> bool) {
        for _ in 0..250 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    }

    /// Let a completed window settle — no further sends may arrive.
    async fn quiet() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    /// (a) 100-event burst to an OFFLINE account ⇒ exactly ONE push, carrying
    /// the latest LSN (the doorbell is a wake-up; the durable checkpoint is
    /// the correctness mechanism).
    #[tokio::test]
    async fn burst_to_offline_account_yields_exactly_one_push() {
        let (rail, _registry, svc) = harness(None).await;
        burst(&svc).await;
        soon(|| !rail.sends.lock().unwrap().is_empty()).await;
        quiet().await;
        let sends = rail.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 1, "100-event burst must collapse to one push");
        assert_eq!(sends[0].1, "dev-e2e");
        assert_eq!(
            sends[0].2,
            PushPayload::Silent {
                table: "tasks".into(),
                lsn: Lsn::new(100)
            }
        );
    }

    /// (b) ONLINE account ⇒ ZERO pushes — the socket is the transport; a
    /// push would double-signal a client that is already receiving.
    #[tokio::test]
    async fn online_account_gets_no_push() {
        // A recording sink that never drops: the session is healthy.
        struct OkSink;
        #[async_trait]
        impl EventSink for OkSink {
            async fn deliver(&self, _event: Arc<ReplicationEvent>) -> DeliveryDecision {
                DeliveryDecision::Delivered
            }
        }
        let (rail, _registry, svc) = harness(Some(Arc::new(OkSink))).await;
        burst(&svc).await;
        quiet().await;
        assert!(
            rail.sends.lock().unwrap().is_empty(),
            "an online account must not be doorbelled"
        );
    }

    /// (c) `Dropped`-but-online ⇒ ZERO pushes — `Dropped` is slow-client
    /// backpressure, NOT presence (ADR-0037 §4); pushing a draining socket
    /// double-signals a client that is catching up.
    #[tokio::test]
    async fn dropped_but_online_account_gets_no_push() {
        let (rail, _registry, svc) = harness(Some(Arc::new(DroppingSink))).await;
        burst(&svc).await;
        quiet().await;
        assert!(
            rail.sends.lock().unwrap().is_empty(),
            "'Dropped' is backpressure, not offline-presence"
        );
    }

    /// (d) Sign-out: the token deregistered through the REST route receives
    /// nothing afterwards — and it DID receive a push before deregistration,
    /// proving the route (not the fixture) removed it.
    #[tokio::test]
    async fn signout_deregisters_token_via_rest_route() {
        let (rail, registry, svc) = harness(None).await;

        // Phase 1: pre-sign-out, the burst doorbells the device.
        burst(&svc).await;
        soon(|| !rail.sends.lock().unwrap().is_empty()).await;
        quiet().await;
        assert_eq!(rail.sends.lock().unwrap().len(), 1);

        // Sign-out: DELETE /push-tokens/{token} through the real handler,
        // with the same JWT auth path the route uses.
        let state = PushApiState {
            auth: Arc::new(FixedAuth(Principal::new("u1", "t1"))),
            registry: registry.clone(),
            tenant_column: Some("org_id".into()),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer jwt-signout".parse().unwrap(),
        );
        let status = push_api::delete_push_token(
            axum::extract::State(state),
            axum::extract::Path("dev-e2e".to_string()),
            headers,
        )
        .await
        .expect("deregistration succeeds");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        assert!(
            registry
                .list_by_account("t1", "u1")
                .await
                .unwrap()
                .is_empty(),
            "the REST route must have removed the token row"
        );

        // Phase 2: a fresh burst (past the previous window) reaches nothing.
        burst(&svc).await;
        quiet().await;
        assert_eq!(
            rail.sends.lock().unwrap().len(),
            1,
            "no push to the deregistered token"
        );
    }
}
