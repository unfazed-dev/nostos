use super::session::SubscribeRequest;
use nostos_application::ActiveRuleset;
use nostos_domain::SyncMode;

// Legacy (pre-D2) request: no `rules_checksum` on the wire, so the gate
// takes the composed-epoch fallback. D2 tests build `SubscribeRequest`
// directly with `client_rules_checksum: Some(_)`.
pub(super) fn req(table: &str, client_epoch: Option<u64>, resume: Option<u64>) -> SubscribeRequest {
    SubscribeRequest {
        table: table.into(),
        filters: Vec::new(),
        where_sql: None,
        resume_lsn: resume,
        client_epoch,
        client_rules_checksum: None,
    }
}

pub(super) fn toggles_rules(
    table: &str,
    sync: bool,
    scope: Option<&str>,
) -> nostos_domain::SyncRules {
    nostos_domain::SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode: SyncMode::Toggles,
        tables: vec![nostos_domain::TableRule {
            table: table.into(),
            sync,
            scope: scope.map(str::to_string),
        }],
        hand: Vec::new(),
        streams: Vec::new(),
    }
}

pub(super) fn stream_ruleset(name: &str, table: &str, template: &str) -> ActiveRuleset {
    ActiveRuleset::compile(&nostos_domain::SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode: SyncMode::All,
        tables: vec![],
        hand: vec![],
        streams: vec![nostos_domain::StreamRule {
            name: name.into(),
            table: table.into(),
            template: template.into(),
        }],
    })
    .unwrap()
}
