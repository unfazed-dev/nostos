//! Direct-mode SQL generation — the server-side half of
//! `docs/plans/direct-mode-sync-protocol.md` (step 6).
//!
//! Direct mode has no Nostos server: the device pulls from the client's own
//! Postgres through PostgREST and is woken by a Realtime broadcast. Everything
//! that makes that safe lives in the database, so `nostos link --mode direct`
//! emits it as one re-runnable SQL file: the `nostos.changes` log, a per-table
//! trigger that appends to it inside the writing transaction, `nostos_pull`,
//! `nostos_increment`, the broadcast doorbell, RLS and the grants.
//!
//! ## Why the refusal is the interesting part
//!
//! The log holds row images from many tables, so **one** policy on
//! `nostos.changes` has to say what N per-table policies say. The trigger
//! stamps a single `scope` text column and RLS compares it to the caller's
//! claims — which only works when a table's rule is exactly
//! `<column> = claims.<field>`. Anything else (a literal term, an inequality,
//! two AND-ed terms, a join) cannot be collapsed into one column, so
//! [`plan`] refuses it by name instead of generating a policy that silently
//! shows the wrong rows. That refusal is cost #2 in the plan, made executable.
//!
//! ## Why `nostos_pull` lives in `public`, not in `nostos`
//!
//! Supabase's default exposed schemas are `public, graphql_public`; a third
//! schema is reachable only with a `Content-Profile` header, and only after an
//! operator ticks it into "Exposed schemas" in the dashboard. Putting the two
//! entry points in `public` under a `nostos_` prefix removes both: the client
//! posts to `/rest/v1/rpc/nostos_pull` with no extra header, and the log table
//! itself stays off the REST API entirely — there is no `GET /rest/v1/changes`
//! to get the grants wrong on.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{bail, Result};
use nostos_domain::{ScopeExpr, ScopeOp, ScopeValue, SyncMode, SyncRules};

/// The primary-key column direct mode assumes on every synced table. Matches
/// `nostos_client::postgrest`'s `PK_COLUMN` and `PendingWrite`'s v1 convention.
pub const PK_COLUMN: &str = "id";

/// The scope value stamped on rows from a table the operator declared public.
pub const PUBLIC_SCOPE: &str = "public";

/// How long `nostos.prune()` keeps change rows by default. A device offline
/// longer than this has to re-snapshot (plan cost #4).
pub const DEFAULT_RETENTION: &str = "7 days";

/// `.nostos/direct.sql` — the generated file, relative to `.nostos/`.
pub const OUTPUT_FILE: &str = "direct.sql";

/// Schemas and tables the change-log trigger must never be attached to.
/// `net` is `pg_net`'s own request/response queue — instrumenting it would
/// make every push attempt a change row, which fires another push. `nostos`'s
/// own tables are the machinery itself, and `device_presence` in particular is
/// written on every heartbeat: logging it would turn a liveness ping into
/// fan-out traffic for every device in the scope.
pub const RESERVED_TABLES: &[&str] = &[
    "changes",
    "retention",
    "push_tokens",
    "device_presence",
    "push_cooldown",
    "push_config",
    "push_templates",
    "http_request_queue",
    "_http_response",
];

/// Where the doorbell-for-a-sleeping-device path posts. `None` = no push
/// section is generated at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfig {
    /// The Edge Function URL `pg_net` posts to.
    pub endpoint: String,
    /// How long after a heartbeat a device still counts as awake.
    pub presence_window: String,
    /// The floor between two pushes to one scope. Also the `pg_net` rate
    /// guard: without it 10,000 write transactions are 10,000 HTTP requests.
    pub cooldown: String,
    /// `nostos link --visible`: the tables whose changes arrive as the
    /// notification itself rather than as a silent doorbell.
    pub templates: Vec<PushTemplate>,
}

/// One `nostos.push_templates` row — a visible (or, with a category, action)
/// push per change to `table`, ADR-0037 §2b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTemplate {
    pub table: String,
    pub title: String,
    pub body: String,
    pub category: Option<String>,
    pub route: Option<String>,
    /// The `[k=v,…]` presentation options (ADR-0047), `{col}` filled by the
    /// Edge Function like title/body.
    pub options: nostos_infra::push::PushOptions,
}

/// Parse one `--visible` spec. The grammar is server mode's
/// `NOSTOS_PUSH_TABLES` visible/action entries, so one line of config moves
/// between the modes unchanged: `table:visible[@/route/{id}][[k=v,…]]:<title>:<body>`
/// or `table:action[@/route/{id}][[k=v,…]]:<category>:<title>:<body>`. The body is
/// the greedy remainder and may contain colons; the options group is
/// nostos-infra's `take_options`, the one parser both modes share.
///
/// ponytail: a second parser of the grammar in nostos-server's
/// `parse_push_tables`; move both into nostos-infra when a third caller shows.
///
/// # Errors
/// [`anyhow::Error`] naming the spec when it is not one of the two shapes, or
/// a table, category or route breaks the identifier/route rules.
pub fn parse_visible(spec: &str) -> Result<PushTemplate> {
    let shape = "expected table:visible[@/route]:<title>:<body> or \
                 table:action[@/route]:<category>:<title>:<body>";
    let Some((table, rest)) = spec.split_once(':') else {
        bail!("--visible {spec:?}: {shape}");
    };
    let (rest, options) = nostos_infra::push::take_options(rest)
        .map_err(|e| anyhow::anyhow!("--visible {spec:?}: {e}"))?;
    let Some((mode, rest)) = rest.split_once(':') else {
        bail!("--visible {spec:?}: {shape}");
    };
    let (mode, route) = match mode.split_once('@') {
        Some((m, r)) => (m, Some(r.trim().to_string())),
        None => (mode, None),
    };
    let (category, rest) = match mode.trim() {
        "visible" => (None, rest),
        "action" => match rest.split_once(':') {
            Some((c, r)) => (Some(c.trim().to_string()), r),
            None => bail!("--visible {spec:?}: {shape}"),
        },
        other => bail!("--visible {spec:?}: mode {other:?} \u{2014} {shape}"),
    };
    let Some((title, body)) = rest.split_once(':') else {
        bail!("--visible {spec:?}: {shape}");
    };
    let table = table.trim();
    if !is_identifier(table) {
        bail!("--visible {spec:?}: table {table:?} must match ^[a-z_][a-z0-9_]*$");
    }
    // The category is the contract with the app's registered notification
    // categories, so a typo must fail here, not as a button-less banner.
    if let Some(c) = category.as_deref().filter(|c| !is_identifier(c)) {
        bail!("--visible {spec:?}: category {c:?} must match ^[a-z_][a-z0-9_]*$");
    }
    if let Some(r) = route
        .as_deref()
        .filter(|r| !r.starts_with('/') || r.chars().any(char::is_whitespace))
    {
        bail!("--visible {spec:?}: route {r:?} must start with '/' and hold no whitespace");
    }
    Ok(PushTemplate {
        table: table.to_string(),
        title: title.trim().to_string(),
        body: body.trim().to_string(),
        category,
        route,
        options,
    })
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('_' | 'a'..='z'))
        && chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The `nostos.push_templates` rows, as the flags say they are: the set is
/// replaced, not merged, so dropping a `--visible` and re-applying stops that
/// table's banners.
#[must_use]
pub fn templates_sql(templates: &[PushTemplate]) -> String {
    let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
    let opt = |v: &Option<String>| v.as_deref().map_or("null".to_string(), lit);
    let mut s = String::from("delete from nostos.push_templates;\n");
    for t in templates {
        let _ = writeln!(
            s,
            "insert into nostos.push_templates (table_name, title, body, category, route, options) \
             values ({}, {}, {}, {}, {}, {});",
            lit(&t.table),
            lit(&t.title),
            lit(&t.body),
            opt(&t.category),
            opt(&t.route),
            lit(&serde_json::to_string(&t.options).unwrap_or_else(|_| "{}".into()))
        );
    }
    s
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            presence_window: "90 seconds".to_string(),
            cooldown: "30 seconds".to_string(),
            templates: Vec::new(),
        }
    }
}

/// How one table's rows are scoped in the shared change log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scoping {
    /// `<column> = claims.<claim>` — the only expressible shape.
    Claim { column: String, claim: String },
    /// Declared public with `--public <table>`: every authenticated device
    /// reads it. Never inferred — an unscoped table is refused instead.
    Public,
}

/// One table in the generated migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectTable {
    pub table: String,
    pub scoping: Scoping,
}

/// Turn `nostos_rules.toml` into a direct-mode table plan, refusing anything
/// whose visibility cannot be expressed as a single scope column.
///
/// # Errors
/// [`anyhow::Error`] naming the offending table when a scope is unexpressible,
/// when a synced table is neither scoped nor declared public, when a
/// `--public` table is not synced, when sync streams are configured (they are
/// server-mode only), or when an identifier is not a plain lowercase SQL name.
pub fn plan(rules: &SyncRules, public_tables: &[String]) -> Result<Vec<DirectTable>> {
    if !rules.streams.is_empty() {
        bail!(
            "sync streams are server-mode only: a stream is a server-held predicate \
             template parameterized per client, and direct mode has no server to hold \
             it. Remove the [streams.*] section or link in server mode."
        );
    }

    let active: Vec<(&str, Option<&str>)> = match rules.mode {
        SyncMode::All => rules
            .tables
            .iter()
            .map(|t| (t.table.as_str(), t.scope.as_deref()))
            .collect(),
        SyncMode::Toggles => rules
            .tables
            .iter()
            .filter(|t| t.sync)
            .map(|t| (t.table.as_str(), t.scope.as_deref()))
            .collect(),
        SyncMode::Hand => rules
            .hand
            .iter()
            .map(|r| (r.table.as_str(), r.scope.as_deref()))
            .collect(),
    };
    if active.is_empty() {
        bail!(
            "no synced tables in nostos_rules.toml (sync_mode = {}) — run \
             `nostos rules init` first; direct mode needs an explicit table list to \
             generate triggers for",
            rules.mode.as_str()
        );
    }

    let public: BTreeSet<&str> = public_tables.iter().map(String::as_str).collect();
    let synced: BTreeSet<&str> = active.iter().map(|(t, _)| *t).collect();
    if let Some(stray) = public.iter().find(|t| !synced.contains(*t)) {
        bail!("`--public {stray}` names a table that is not synced in nostos_rules.toml");
    }

    let mut out = Vec::with_capacity(active.len());
    for (table, scope) in active {
        check_ident(table, "table name")?;
        if RESERVED_TABLES.contains(&table) {
            bail!(
                "table `{table}` is one of direct mode's own: instrumenting it would feed                  the machinery back into itself (a push attempt logging a change that                  fires another push, or a heartbeat fanning out to every device).                  Remove it from the sync set."
            );
        }
        let scope = scope.map(str::trim).filter(|s| !s.is_empty());
        let scoping = match (scope, public.contains(table)) {
            (Some(text), true) => bail!(
                "table `{table}` is declared `--public` but also carries the scope \
                 `{text}` — pick one"
            ),
            (Some(text), false) => claim_scoping(table, text)?,
            (None, true) => Scoping::Public,
            (None, false) => bail!(
                "table `{table}` is synced with no scope. In server mode that means \
                 \"whole table, tenant-scoped by the session\"; direct mode has no \
                 session to scope it by, so the rows would be readable by every \
                 authenticated device. Give it a scope (`<column> = claims.<field>`) \
                 or say so out loud with `--public {table}`."
            ),
        };
        out.push(DirectTable {
            table: table.to_string(),
            scoping,
        });
    }
    Ok(out)
}

/// The one expressible shape: a single `column = claims.field` comparison.
fn claim_scoping(table: &str, text: &str) -> Result<Scoping> {
    let refusal = |why: &str| {
        anyhow::anyhow!(
            "table `{table}`: scope `{text}` cannot be expressed in direct mode — \
             {why}. The change log has ONE scope column shared by every table, so a \
             direct-mode scope must be exactly `<column> = claims.<field>`. Rewrite \
             the rule, drop the table from the sync set, or run this project in \
             server mode where the predicate engine evaluates the full grammar."
        )
    };
    let expr = ScopeExpr::parse(text).map_err(|e| refusal(&e.to_string()))?;
    let [term] = expr.terms.as_slice() else {
        return Err(refusal(
            "it AND-composes several comparisons, which one column cannot carry",
        ));
    };
    if term.op != ScopeOp::Eq {
        return Err(refusal(
            "only `=` can be answered by comparing one stamped value",
        ));
    }
    let ScopeValue::Claim(claim) = &term.value else {
        return Err(refusal(
            "it compares against a literal, which filters rows rather than scoping \
             them — a row that later stops matching would never be sent as a removal",
        ));
    };
    check_ident(&term.column, "scope column")?;
    check_ident(claim, "claim name")?;
    Ok(Scoping::Claim {
        column: term.column.clone(),
        claim: claim.clone(),
    })
}

/// Generated SQL interpolates identifiers directly, so they are held to plain
/// lowercase `[a-z_][a-z0-9_]*` rather than quoted. A name outside that is
/// rejected loudly instead of silently quoted into something that no longer
/// matches what the operator typed.
fn check_ident(name: &str, what: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        bail!(
            "{what} `{name}` is not a plain lowercase SQL identifier \
             (a-z, 0-9, _); direct mode generates unquoted SQL and will not guess a quoting"
        )
    }
}

/// Render the whole migration. Pure: same input, same bytes, every time —
/// which is what lets an operator diff two runs after a rules edit.
#[must_use]
pub fn render(tables: &[DirectTable], retention: &str) -> String {
    render_with_push(tables, retention, None)
}

/// [`render`] plus the push path. Separate because push is opt-in: without
/// `--push <url>` none of it is emitted, so the default schema stays the four
/// objects the protocol actually needs.
#[must_use]
pub fn render_with_push(
    tables: &[DirectTable],
    retention: &str,
    push: Option<&PushConfig>,
) -> String {
    let mut s = String::with_capacity(8 * 1024);
    let claims: BTreeSet<&str> = tables
        .iter()
        .filter_map(|t| match &t.scoping {
            Scoping::Claim { claim, .. } => Some(claim.as_str()),
            Scoping::Public => None,
        })
        .collect();
    let any_public = tables.iter().any(|t| t.scoping == Scoping::Public);

    header(&mut s, tables);
    log_table(&mut s);
    current_scopes(&mut s, &claims, any_public);
    log_trigger_fn(&mut s);
    per_table_triggers(&mut s, tables);
    pull_fn(&mut s);
    snapshot_fn(&mut s, tables);
    increment_fn(&mut s, tables);
    doorbell(&mut s);
    policies_and_grants(&mut s);
    prune(&mut s, retention);
    if let Some(cfg) = push {
        push_path(&mut s, cfg);
    }
    s.push_str("\ncommit;\n");
    s
}

fn header(s: &mut String, tables: &[DirectTable]) {
    let _ = writeln!(
        s,
        "-- Generated by `nostos link --mode direct`. Re-runnable: every object is\n\
         -- created with `if not exists` or dropped first, so applying it twice is a\n\
         -- no-op. Regenerate after editing nostos_rules.toml and diff the two files.\n\
         --\n\
         -- Protocol: docs/plans/direct-mode-sync-protocol.md\n\
         -- Client:   nostos_core::pull (cursor) + nostos_client::postgrest (transport)\n\
         --\n\
         -- Synced tables:"
    );
    for t in tables {
        let _ = match &t.scoping {
            Scoping::Claim { column, claim } => {
                writeln!(
                    s,
                    "--   public.{} -> scope {column} = claims.{claim}",
                    t.table
                )
            }
            Scoping::Public => writeln!(
                s,
                "--   public.{} -> PUBLIC (every authenticated device reads it)",
                t.table
            ),
        };
    }
    s.push_str("\nbegin;\n");
    legacy_rename(s);
    s.push_str("\ncreate schema if not exists nostos;\n");
}

/// ADR-0048: a project linked before the rename has a `cairn` schema, the
/// `public.cairn_*` RPCs and `cairn_*` triggers and policies. Renamed in place,
/// not rebuilt, so the change log, its sequence, the push tokens and config
/// keep their rows and every trigger keeps firing. Bodies still name the old
/// schema, so each function in the renamed set is re-created with the names
/// swapped; the rest of the file then replaces the ones it knows.
fn legacy_rename(s: &mut String) {
    s.push_str(
        r"
-- ADR-0048: a project linked before the rename is renamed in place, so the
-- change log and push registry keep their rows. Runs once: afterwards there is
-- no old schema left to find.
do $rename$
declare
  old constant text := 'cairn'; -- rename:hold — the pre-rename identity (ADR-0048)
  r record;
begin
  if to_regnamespace(old) is null or to_regnamespace('nostos') is not null then
    return;
  end if;
  execute format('alter schema %I rename to nostos', old);
  for r in select p.oid::regprocedure as fn, p.proname from pg_proc p
            where p.pronamespace = 'public'::regnamespace and p.proname like old || '\_%' loop
    execute format('alter function %s rename to %I', r.fn, 'nostos' || substr(r.proname, length(old) + 1));
  end loop;
  for r in select p.oid from pg_proc p
            where p.prokind in ('f', 'p')
              and (p.pronamespace = 'nostos'::regnamespace
                   or (p.pronamespace = 'public'::regnamespace and p.proname like 'nostos\_%')) loop
    execute replace(pg_get_functiondef(r.oid), old, 'nostos');
  end loop;
  for r in select t.tgname, t.tgrelid::regclass as rel from pg_trigger t
            where not t.tgisinternal and t.tgname like old || '\_%' loop
    execute format('alter trigger %I on %s rename to %I', r.tgname, r.rel, 'nostos' || substr(r.tgname, length(old) + 1));
  end loop;
  for r in select p.polname, p.polrelid::regclass as rel from pg_policy p
            where p.polname like old || '\_%' loop
    execute format('alter policy %I on %s rename to %I', r.polname, r.rel, 'nostos' || substr(r.polname, length(old) + 1));
  end loop;
  -- A pg_cron job still calling the old schema's prune() would fail every run.
  if to_regclass('cron.job') is not null then
    for r in select jobid, command from cron.job where command like '%' || old || '.%' loop
      perform cron.alter_job(r.jobid, command := replace(r.command, old || '.', 'nostos.'));
    end loop;
  end if;
end $rename$;
",
    );
}

fn log_table(s: &mut String) {
    s.push_str(
        r#"
-- The transactional outbox. The trigger below appends to it INSIDE the writing
-- transaction, so a data change and its change record commit together or not at
-- all. `xid` is what makes a cross-table transaction reassemblable on the
-- device; `seq` only orders rows within one.
create table if not exists nostos.changes (
  seq        bigserial primary key,
  xid        xid8        not null default pg_current_xact_id(),
  table_name text        not null,
  pk         text        not null,
  op         text        not null,
  "row"      jsonb,
  scope      text,
  logged_at  timestamptz not null default now()
);
create index if not exists changes_xid_seq_idx    on nostos.changes (xid, seq);
create index if not exists changes_logged_at_idx  on nostos.changes (logged_at);

-- How far the log has been pruned. Without this a device that was away longer
-- than the retention window resumes from a horizon whose rows are gone and
-- gets a shorter answer instead of an error -- the rows in the gap would
-- simply never arrive. One row, so the guard in nostos_pull is a lookup.
create table if not exists nostos.retention (
  id           int  primary key default 1 check (id = 1),
  pruned_below xid8 not null default '0'::xid8
);
insert into nostos.retention (id) values (1) on conflict do nothing;
"#,
    );
}

fn current_scopes(s: &mut String, claims: &BTreeSet<&str>, any_public: bool) {
    s.push_str(
        r#"
-- Every scope value is namespaced `<claim>:<value>` so two claims can never
-- collide in the one shared column: a user whose `sub` happens to equal another
-- tenant's `org_id` still cannot read their rows.
--
-- security definer, and the reason is not authority: it takes no arguments and
-- reads only the caller's own request GUC, so it can return nothing the caller
-- is not already entitled to. What it buys is not depending on the caller
-- holding USAGE on `auth` — hosted Supabase grants that to `authenticated`, a
-- self-hosted PostgREST need not, and the failure mode there is every pull
-- erroring with "permission denied for schema auth" (caught by the pg e2e).
create or replace function nostos.current_scopes()
returns text[] language sql stable security definer set search_path = '' as $fn$
  select array_remove(array[
"#,
    );
    let mut elems: Vec<String> = claims
        .iter()
        .map(|c| format!("    '{c}:' || (auth.jwt() ->> '{c}')"))
        .collect();
    if any_public {
        elems.push(format!("    '{PUBLIC_SCOPE}'"));
    }
    s.push_str(&elems.join(",\n"));
    s.push_str("\n  ], null);\n$fn$;\n");
}

fn log_trigger_fn(s: &mut String) {
    let _ = write!(
        s,
        r#"
-- security definer: the log is append-only to everyone, including the user
-- whose write produced the row. Devices get SELECT and nothing else, so no
-- client can forge, amend or erase history.
--
-- Scope-change handling: an UPDATE that moves a row to another scope logs TWO
-- records — a delete under the old scope and the update under the new one.
-- Without it the losing tenant keeps a row on-device forever, since a row they
-- can no longer see can never be sent to them again.
create or replace function nostos.log_change() returns trigger
language plpgsql security definer set search_path = '' as $fn$
declare
  v_img       jsonb;
  v_old       jsonb;
  v_scope     text;
  v_old_scope text;
begin
  if tg_op = 'DELETE' then
    v_img := to_jsonb(old);
  else
    v_img := to_jsonb(new);
  end if;
  if tg_op = 'UPDATE' then
    v_old := to_jsonb(old);
  end if;

  if array_length(tg_argv, 1) = 2 then
    v_scope := tg_argv[1] || ':' || (v_img ->> tg_argv[0]);
    if v_old is not null then
      v_old_scope := tg_argv[1] || ':' || (v_old ->> tg_argv[0]);
    end if;
  else
    v_scope := '{PUBLIC_SCOPE}';
    v_old_scope := v_scope;
  end if;

  if v_old_scope is not null and v_old_scope is distinct from v_scope then
    insert into nostos.changes (table_name, pk, op, "row", scope)
    values (tg_table_name, v_old ->> '{PK_COLUMN}', 'delete', null, v_old_scope);
  end if;

  insert into nostos.changes (table_name, pk, op, "row", scope)
  values (
    tg_table_name,
    v_img ->> '{PK_COLUMN}',
    lower(tg_op),
    case when tg_op = 'DELETE' then null else v_img end,
    v_scope
  );

  if tg_op = 'DELETE' then
    return old;
  end if;
  return new;
end;
$fn$;
"#
    );
}

fn per_table_triggers(s: &mut String, tables: &[DirectTable]) {
    s.push('\n');
    for t in tables {
        let name = &t.table;
        let args = match &t.scoping {
            Scoping::Claim { column, claim } => format!("('{column}', '{claim}')"),
            Scoping::Public => "()".to_string(),
        };
        let _ = write!(
            s,
            "drop trigger if exists nostos_log_{name} on public.{name};\n\
             create trigger nostos_log_{name}\n  \
             after insert or update or delete on public.{name}\n  \
             for each row execute function nostos.log_change{args};\n\n"
        );
    }
}

fn pull_fn(s: &mut String) {
    s.push_str(
        r#"
-- The whole read path, in one call so the snapshot and the rows come from one
-- transaction. `pg_snapshot_xmin` is the lowest xid still in progress: every
-- xid below it is settled, so nothing can ever appear beneath it later. That
-- makes the horizon a gapless checkpoint no clock can skew.
--
-- The page is max_txns TRANSACTIONS, not rows, and that is load-bearing. A row
-- limit can cut a transaction in half; the client would then have to hold the
-- tail back and re-read it, and since `since` is inclusive the re-read returns
-- the same page and cuts the same tail forever. `greatest(max_txns, 2)`
-- guarantees a full page spans two xids, so the cursor always advances.
--
-- security invoker: RLS on nostos.changes applies as the calling device.
--
-- Returns ONE jsonb array, not a set of rows, and that is not a style choice.
-- PostgREST caps a set-returning response at `db-max-rows` (1000 on a stock
-- Supabase project) and announces the cut with nothing but `content-range:
-- 0-999/*` on a 200 -- no 206, and `Range`/`offset` are ignored on an RPC, so
-- there is no paging past it either. A page of 200 transactions can easily
-- exceed 1000 rows; the tail would be dropped and the device would then store
-- the horizon PAST rows it never saw. Silent, permanent loss. A scalar result
-- is one row however big it gets, so the cap cannot reach it, and PostgREST
-- renders a jsonb scalar as the bare array the client already parses.
-- (Measured 2026-09-22 against a real project: a 1010-row snapshot came back
-- as 1000 rows, 200 OK, two whole tables missing.)
--
-- Dropped first, not just replaced: `create or replace` refuses to change a
-- function's return type, so a project still carrying the set-returning
-- version would fail this file rather than upgrade. The grants below are
-- re-issued after, which is what a drop costs.
drop function if exists public.nostos_pull(xid8, int);
create or replace function public.nostos_pull(since xid8, max_txns int default 200)
returns jsonb
language plpgsql stable security invoker set search_path = '' as $fn$
declare
  v_pruned xid8;
  v_page   jsonb;
begin
  -- A device resuming from a horizon that has been pruned away must be TOLD,
  -- not quietly given a shorter answer: the rows in the gap can never arrive
  -- any other way. PostgREST turns a `PTxyz` sqlstate into HTTP xyz, so this
  -- reaches the client as 410 Gone -- re-snapshot and reset the horizon.
  select r.pruned_below into v_pruned from nostos.retention r;
  if since <> '0'::xid8 and v_pruned is not null and since <= v_pruned then
    raise sqlstate 'PT410' using
      message = 'nostos: the change log has been pruned past this horizon',
      detail  = 'the device was offline longer than the retention window',
      hint    = 'reset the stored horizon and re-snapshot the synced tables';
  end if;

  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon),
  page as (
    select distinct c.xid
    from nostos.changes c, h
    where c.xid >= since and c.xid < h.horizon
    order by c.xid
    limit greatest(max_txns, 2)
  )
  select coalesce(jsonb_agg(to_jsonb(r) order by r.xid, r.seq), '[]'::jsonb)
    into v_page
  from (
    select h.horizon, c.seq, c.xid, c.table_name, c.pk, c.op, c."row"
    from nostos.changes c
    join page p on p.xid = c.xid
    cross join h
  ) r;
  return v_page;
end;
$fn$;
"#,
    );
}

/// `public.nostos_snapshot()` — the only way back from a 410.
///
/// `nostos_pull` refuses a horizon below the pruned window, which is correct: a
/// short answer would be indistinguishable from "nothing happened" and the rows
/// in the gap would never arrive. But a refusal the client cannot act on is a
/// device bricked by going on holiday. This is the act: the CURRENT rows of
/// every synced table plus a horizon to resume from, all from ONE statement, so
/// the snapshot is as cross-table consistent as a pull is.
///
/// One header row per table (`pk` null) so the payload says which tables it
/// covers even when a table is empty — otherwise a table emptied server-side
/// while the device was away would keep its stale local rows forever.
fn snapshot_fn(s: &mut String, tables: &[DirectTable]) {
    let mut branches = String::new();
    for t in tables {
        let _ = write!(
            branches,
            "    union all\n    select h.horizon, '{table}'::text, null::text, null::jsonb from h\n    \
             union all\n    select h.horizon, '{table}'::text, r.{pk}::text, to_jsonb(r) \
             from public.{table} r, h\n",
            table = t.table,
            pk = PK_COLUMN,
        );
    }
    let _ = write!(
        s,
        r#"
-- The re-snapshot path. `nostos_pull` answers a horizon below the retention
-- window with 410; this is what the client does about it. Returns the current
-- rows of every synced table AND the horizon to resume from, from one
-- statement -- so, like a pull, it is one consistent cross-table view.
--
-- The first row for each table has a null `pk`: it announces that the table is
-- part of this snapshot. Without it an empty table is indistinguishable from a
-- table the snapshot forgot, and the device would keep rows the server no
-- longer has.
--
-- security invoker, so RLS on each base table decides what is in the snapshot.
-- That is the same authority that decides what a pull returns, which is what
-- makes the two interchangeable.
--
-- One jsonb array rather than a set of rows, for the reason spelled out over
-- `nostos_pull`: PostgREST silently truncates a set-returning RPC at
-- `db-max-rows`, and a snapshot is the one call guaranteed to be big.
--
-- ponytail: the whole snapshot is materialised in one value, so its ceiling is
-- what the server and the device can each hold at once. Upgrade path when a
-- table outgrows that: take a keyset (`p_after_table`, `p_after_pk`) plus a
-- limit, and have the client resume its pull from the FIRST page's horizon --
-- anything that changed mid-pagination is then re-delivered by the log, the
-- same way a base backup is healed by the WAL that follows it.
--
-- Dropped first for the same reason as `nostos_pull`: a return type cannot be
-- replaced in place.
drop function if exists public.nostos_snapshot();
create or replace function public.nostos_snapshot()
returns jsonb
language sql stable security invoker set search_path = '' as $fn$
  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon),
  snap as (
    select h.horizon, null::text as table_name, null::text as pk, null::jsonb as "row" from h
{branches}  )
  select coalesce(jsonb_agg(to_jsonb(snap)), '[]'::jsonb) from snap
$fn$;
"#
    );
}

fn increment_fn(s: &mut String, tables: &[DirectTable]) {
    let allowed = tables
        .iter()
        .map(|t| format!("'{}'", t.table))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = write!(
        s,
        r#"
-- The one write op PostgREST cannot express. ADR-0030's no-lost-update
-- guarantee is that POSTGRES serializes concurrent increments (`set x = x + ?`);
-- a PATCH body carries literals, so routing an increment through one would put
-- the read back on the device and reinstate the lost update.
--
-- security invoker, so RLS on the target table is what authorizes the write.
-- The table allow-list and `%I` quoting are belt and braces: without them this
-- would be a general "update any column of any table" gadget.
create or replace function public.nostos_increment(
  p_table text, p_pk text, p_field text, p_delta numeric)
returns void language plpgsql security invoker set search_path = '' as $fn$
declare
  v_pk_type text;
begin
  if p_table not in ({allowed}) then
    raise exception 'nostos_increment: table % is not synced', p_table
      using errcode = '42501';
  end if;

  select a.atttypid::regtype::text into v_pk_type
  from pg_catalog.pg_attribute a
  where a.attrelid = format('public.%I', p_table)::regclass
    and a.attname = '{PK_COLUMN}'
    and a.attnum > 0;
  if v_pk_type is null then
    raise exception 'nostos_increment: public.% has no {PK_COLUMN} column', p_table
      using errcode = '42703';
  end if;

  -- The cast is on the parameter, not the column, so the primary-key index is
  -- still usable.
  execute format(
    'update public.%I set %I = coalesce(%I, 0) + $1 where {PK_COLUMN} = $2::text::%s',
    p_table, p_field, p_field, v_pk_type)
  using p_delta, p_pk;
end;
$fn$;
"#
    );
}

fn doorbell(s: &mut String) {
    s.push_str(
        r#"
-- The doorbell carries no data: it says "there is something new for this
-- scope", the device pulls, and the pull is the only thing that moves rows.
-- That is what lets one protocol serve every SDK — a client needs one HTTPS
-- POST and one WebSocket, nothing that understands row shapes.
--
-- ponytail: one broadcast per logged row. realtime.send only appends to
-- realtime.messages, so the cost is a row, and the device coalesces rings into
-- one pull anyway. Upgrade path if a bulk import floods it: dedupe per
-- transaction (a deferred constraint trigger, or a statement trigger over a
-- transition table once the log trigger batches).
create or replace function nostos.ring() returns trigger
language plpgsql security definer set search_path = '' as $fn$
begin
  perform realtime.send(
    '{}'::jsonb,                                  -- payload: deliberately empty
    'nostos_ring',                                 -- event
    'nostos:' || coalesce(new.scope, 'unscoped'),  -- topic (channel joins as realtime:<topic>)
    true                                          -- private: RLS below authorizes it
  );
  return null;
end;
$fn$;

drop trigger if exists nostos_changes_ring on nostos.changes;
create trigger nostos_changes_ring
  after insert on nostos.changes
  for each row execute function nostos.ring();
"#,
    );
}

fn policies_and_grants(s: &mut String) {
    s.push_str(
        r#"
-- RLS is the ONLY thing authorizing a direct-mode read. That is the security
-- argument, not a caveat: a forbidden row is refused by Postgres rather than by
-- a service the developer has to trust.
alter table nostos.changes enable row level security;

drop policy if exists nostos_changes_read on nostos.changes;
create policy nostos_changes_read on nostos.changes
  for select to authenticated
  using (scope = any (nostos.current_scopes()));

-- No insert/update/delete policy exists, and none should: nostos.log_change is
-- security definer, so the trigger writes history and nobody else can.

-- The private Realtime channel. Broadcast-from-database requires Realtime
-- Authorization, which is a policy on realtime.messages — and it is only
-- ENFORCED once "Allow public access" is off in the project's Realtime
-- settings. `nostos doctor --mode direct` checks that; SQL cannot.
drop policy if exists nostos_ring_read on realtime.messages;
create policy nostos_ring_read on realtime.messages
  for select to authenticated
  using (
    realtime.messages.extension = 'broadcast'
    and (select realtime.topic()) like 'nostos:%'
    and substring((select realtime.topic()) from 7) = any (nostos.current_scopes())
  );

grant usage on schema nostos to authenticated;
grant select on nostos.changes to authenticated;
grant select on nostos.retention to authenticated;
grant execute on function nostos.current_scopes() to authenticated;

-- Postgres grants EXECUTE to PUBLIC on every new function, which would hand the
-- whole change log to the anon key. Revoke first, then grant narrowly.
--
-- `from public` alone is NOT enough on Supabase, and this is the kind of thing
-- only a real project shows you: the platform's DEFAULT PRIVILEGES grant
-- EXECUTE to `anon`, `authenticated` and `service_role` BY NAME at creation
-- time, and revoking from the PUBLIC pseudo-role does not touch a grant made to
-- a named role. Every function here came out with `anon=X` until the roles were
-- named. For a `security invoker` function that is only defence in depth (the
-- table grants still hold the line); a `security definer` one bypasses those,
-- so naming the roles is what actually closes it.
revoke all on function public.nostos_pull(xid8, int) from public, anon, authenticated;
revoke all on function public.nostos_snapshot() from public, anon, authenticated;
revoke all on function public.nostos_increment(text, text, text, numeric)
  from public, anon, authenticated;
grant execute on function public.nostos_pull(xid8, int) to authenticated;
grant execute on function public.nostos_snapshot() to authenticated;
grant execute on function public.nostos_increment(text, text, text, numeric) to authenticated;
"#,
    );
}

fn prune(s: &mut String, retention: &str) {
    let _ = write!(
        s,
        r"
-- Retention. A device offline longer than this re-snapshots instead of
-- resuming, so the window is a product decision, not a storage one. Schedule it
-- with pg_cron:
--   select cron.schedule('nostos-prune', '0 * * * *', $$select nostos.prune()$$);
create or replace function nostos.prune(retain interval default interval '{retention}')
returns bigint language plpgsql security definer set search_path = '' as $fn$
declare
  n        bigint;
  v_high   xid8;
begin
  with gone as (
    delete from nostos.changes where logged_at < now() - retain returning xid
  )
  select count(*), max(xid) into n, v_high from gone;
  if v_high is not null then
    update nostos.retention
       set pruned_below = case when pruned_below > v_high then pruned_below else v_high end
     where id = 1;
  end if;
  return n;
end;
$fn$;
"
    );
}

/// The push path: token registry, presence, and the trigger that wakes a
/// device nobody is listening on.
///
/// ## Why presence is checked in SQL, not guessed
///
/// A doorbell only has to reach devices that are *not* already connected — a
/// live device got the Realtime ring milliseconds ago. Server mode knows who
/// is connected because it holds the sockets; direct mode has no server, so
/// the device says so itself with `nostos_heartbeat()`. A stale heartbeat is
/// the only "offline" signal that exists here, and it is a heuristic: a device
/// that dies mid-window is pushed a little late, and one that was awake very
/// recently may be skipped. That is the honest cost of having no server.
///
/// ## Why the cooldown is load-bearing, not politeness
///
/// `pg_net` is transactional — requests are not started until the transaction
/// commits — but it is still one HTTP request per call, and its worker has a
/// finite rate. Ten thousand separate write transactions would be ten thousand
/// requests. The `on conflict … where` below is an atomic per-scope debounce:
/// only the statement that actually updates the row sends, so concurrent
/// writers collapse into one push without a lock or an advisory queue.
fn push_path(s: &mut String, cfg: &PushConfig) {
    let PushConfig {
        endpoint,
        presence_window,
        cooldown,
        templates,
    } = cfg;
    let _ = write!(
        s,
        r#"
-- ===========================================================================
-- Push (`nostos link --mode direct --push <url>`). A killed app that never
-- wakes is indistinguishable from broken sync, so this is not a follow-up.
-- ===========================================================================

create table if not exists nostos.push_config (
  id               int  primary key default 1 check (id = 1),
  endpoint         text not null,
  secret           text,
  presence_window  interval not null default interval '{presence_window}',
  cooldown         interval not null default interval '{cooldown}'
);
insert into nostos.push_config (id, endpoint) values (1, '{endpoint}')
on conflict (id) do update set endpoint = excluded.endpoint;
-- The shared secret the Edge Function checks. Set it out of band, so it is
-- never written into a file that lands in git:
--   update nostos.push_config set secret = '<random>' where id = 1;

create table if not exists nostos.push_tokens (
  scope      text not null,
  platform   text not null check (platform in ('fcm', 'apns', 'webpush')),
  token      text not null,
  updated_at timestamptz not null default now(),
  primary key (scope, platform, token)
);
create index if not exists push_tokens_scope_idx on nostos.push_tokens (scope);

create table if not exists nostos.device_presence (
  scope     text not null,
  device_id text not null,
  last_seen timestamptz not null default now(),
  primary key (scope, device_id)
);
create index if not exists device_presence_seen_idx on nostos.device_presence (scope, last_seen);

create table if not exists nostos.push_cooldown (
  scope        text primary key,
  last_push_at timestamptz not null default now()
);

-- Visible pushes, per table (ADR-0037 §2 `visible`/`action`, the rows of server
-- mode's NOSTOS_PUSH_TABLES). iOS never wakes a user-quit app for a silent
-- doorbell, but it always shows an alert -- so a table the user must hear about
-- gets a row here, and its changes arrive as the notification itself. `{{col}}`
-- in title/body/route is filled from the changed row by the Edge Function; a
-- non-null category makes it an action push. The rows come from `nostos link
-- --visible`, at the end of this section.
create table if not exists nostos.push_templates (
  table_name text primary key,
  title      text not null,
  body       text not null,
  category   text,
  route      text,
  options    jsonb not null default '{{}}'
);
-- `[k=v,…]` presentation options (ADR-0047); added in place on a project
-- linked before they existed.
alter table nostos.push_templates add column if not exists options jsonb not null default '{{}}';

-- security definer, and here that IS the authority: the scope comes from the
-- caller's own JWT via nostos.current_scopes(), never from an argument, so a
-- device cannot register a token against somebody else's scope no matter what
-- it sends.
create or replace function public.nostos_register_push_token(p_platform text, p_token text)
returns void language plpgsql security definer set search_path = '' as $fn$
declare v_scopes text[] := nostos.current_scopes();
begin
  if p_platform not in ('fcm', 'apns', 'webpush') then
    raise sqlstate 'PT400' using message = 'nostos: unknown push platform';
  end if;
  if coalesce(array_length(v_scopes, 1), 0) = 0 then
    raise sqlstate 'PT401' using
      message = 'nostos: no scope in the caller''s claims',
      hint    = 'register the token while signed in';
  end if;
  insert into nostos.push_tokens (scope, platform, token)
  select s, p_platform, p_token from unnest(v_scopes) as s
  on conflict (scope, platform, token) do update set updated_at = now();
end;
$fn$;

create or replace function public.nostos_deregister_push_token(p_token text)
returns void language plpgsql security definer set search_path = '' as $fn$
begin
  delete from nostos.push_tokens
   where token = p_token and scope = any (nostos.current_scopes());
end;
$fn$;

-- How the Edge Function reads the registry -- and the reason it is a function
-- rather than a select on nostos.push_tokens. `nostos` is deliberately not an
-- exposed schema (the same decision that put nostos_pull in `public`), so a
-- service-role client pointed at it gets `500 Invalid schema: nostos` and no
-- push is ever sent. Caught against a real Supabase stack, not by reasoning.
--
-- security definer to reach the unexposed table; granted to `service_role`
-- ONLY, so the anon and authenticated keys cannot enumerate anybody's tokens.
create or replace function public.nostos_push_targets(p_scope text)
returns table (platform text, token text)
language sql stable security definer set search_path = '' as $fn$
  select t.platform, t.token from nostos.push_tokens t where t.scope = p_scope;
$fn$;

-- "I am awake." Cheap enough to send on every foreground and every pull.
create or replace function public.nostos_heartbeat(p_device_id text)
returns void language plpgsql security definer set search_path = '' as $fn$
begin
  insert into nostos.device_presence (scope, device_id)
  select s, p_device_id from unnest(nostos.current_scopes()) as s
  on conflict (scope, device_id) do update set last_seen = now();
end;
$fn$;

create or replace function nostos.wake_absent_devices() returns trigger
language plpgsql security definer set search_path = '' as $fn$
declare
  v_cfg  nostos.push_config%rowtype;
  v_tpl  nostos.push_templates%rowtype;
  v_body jsonb := jsonb_build_object('scope', new.scope);
begin
  select * into v_cfg from nostos.push_config where id = 1;
  if v_cfg.endpoint is null or v_cfg.endpoint = '' then
    return null;
  end if;
  -- pg_net is an extension and may not be installed; `nostos doctor --mode
  -- direct` reports that. Silently skipping beats failing every write.
  if to_regproc('net.http_post') is null then
    return null;
  end if;
  -- Somebody is listening: the Realtime ring already reached them.
  if exists (
    select 1 from nostos.device_presence
     where scope = new.scope and last_seen > now() - v_cfg.presence_window
  ) then
    return null;
  end if;
  select * into v_tpl from nostos.push_templates where table_name = new.table_name;
  if found and new.op <> 'delete' then
    -- A visible push is the news itself, so it skips the debounce: a
    -- debounced banner is a lost banner. The operator chose these tables, and
    -- one request per row of them is the cost of choosing.
    v_body := v_body || jsonb_build_object(
      'row', new.row, 'title', v_tpl.title, 'body', v_tpl.body,
      'category', v_tpl.category, 'route', v_tpl.route, 'options', v_tpl.options);
  else
    -- The debounce, and the advisory lock in front of it is not an
    -- optimization. One shared row per scope means a row lock per scope, held
    -- until the writing transaction commits -- so a long transaction would
    -- block EVERY other writer in that scope. A delayed sync is acceptable; a
    -- blocked write is not. `pg_try_advisory_xact_lock` never waits: a writer
    -- that finds the scope taken skips, which is exactly what the debounce
    -- would have told it to do anyway. (Found by the pg e2e, which deadlocked
    -- without it.)
    if not pg_try_advisory_xact_lock(hashtext('nostos:push:' || new.scope)) then
      return null;
    end if;
    insert into nostos.push_cooldown (scope, last_push_at) values (new.scope, now())
    on conflict (scope) do update set last_push_at = now()
     where nostos.push_cooldown.last_push_at < now() - v_cfg.cooldown;
    if not found then
      return null;
    end if;
  end if;
  perform net.http_post(
    url     := v_cfg.endpoint,
    body    := v_body,
    headers := jsonb_build_object(
      'Content-Type', 'application/json',
      'Authorization', 'Bearer ' || coalesce(v_cfg.secret, '')
    )
  );
  return null;
end;
$fn$;

drop trigger if exists nostos_changes_wake on nostos.changes;
create trigger nostos_changes_wake
  after insert on nostos.changes
  for each row execute function nostos.wake_absent_devices();

-- Devices talk to all of this through the three functions above and nothing
-- else: no direct table access, so there is no policy to get wrong and no way
-- to enumerate another tenant's tokens.
-- Named roles, not just PUBLIC -- see the revoke block above. These are
-- `security definer`, so a leftover `anon=X` is not defence in depth: it is an
-- anonymous caller registering a push token under the `public` scope, or
-- reading another tenant's tokens outright.
revoke all on function public.nostos_register_push_token(text, text)
  from public, anon, authenticated;
revoke all on function public.nostos_deregister_push_token(text)
  from public, anon, authenticated;
revoke all on function public.nostos_heartbeat(text) from public, anon, authenticated;
revoke all on function public.nostos_push_targets(text) from public, anon, authenticated;
grant execute on function public.nostos_register_push_token(text, text) to authenticated;
grant execute on function public.nostos_deregister_push_token(text) to authenticated;
grant execute on function public.nostos_heartbeat(text) to authenticated;
-- Not `authenticated`: the registry is the Edge Function's business only.
grant execute on function public.nostos_push_targets(text) to service_role;
alter table nostos.push_tokens     enable row level security;
alter table nostos.device_presence enable row level security;
alter table nostos.push_cooldown   enable row level security;
alter table nostos.push_templates  enable row level security;
"#
    );
    s.push_str(&templates_sql(templates));
}

// ---------------------------------------------------------------------------
// `nostos doctor --mode direct` — read-only verification of a deployed schema.
// ---------------------------------------------------------------------------

/// How a check came out. `Note` is information the operator needs but that
/// cannot pass or fail — a size, or something SQL simply cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Fail,
    Note,
}

/// One line of `nostos doctor --mode direct` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub verdict: Verdict,
    pub label: String,
}

impl Check {
    fn new(ok: bool, label: impl Into<String>) -> Self {
        Self {
            verdict: if ok { Verdict::Ok } else { Verdict::Fail },
            label: label.into(),
        }
    }
    fn note(label: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Note,
            label: label.into(),
        }
    }
    /// The glyph `doctor` prints.
    #[must_use]
    pub fn glyph(&self) -> char {
        match self.verdict {
            Verdict::Ok => '\u{2713}',
            Verdict::Fail => '\u{2717}',
            Verdict::Note => '\u{2022}',
        }
    }
}

/// Inspect a deployed direct-mode schema. Read-only — every statement here is
/// a `select`, so this is safe to point at production.
///
/// ## The check that justifies the command
///
/// **`nostos_pull` must page by transaction.** A deployed `pull` that pages by
/// *rows* still returns rows, still advances a horizon, and still looks
/// healthy from the device — it just hands out half a transaction, which the
/// client applies atomically and cannot detect as partial. There is no
/// client-side test for it. Reading the deployed function's own source is the
/// only place that bug is visible, so it lives here.
///
/// # Errors
/// [`anyhow::Error`] if a catalog query fails (a missing object is a failed
/// check, not an error).
pub async fn inspect(
    client: &tokio_postgres::Client,
    tables: &[DirectTable],
) -> Result<Vec<Check>> {
    let mut out = Vec::new();

    let log_exists: bool = client
        .query_one("select to_regclass('nostos.changes') is not null", &[])
        .await?
        .get(0);
    out.push(Check::new(log_exists, "nostos.changes exists"));
    if !log_exists {
        out.push(Check::note(
            "nothing else can be checked \u{2014} run `nostos link --mode direct` \
             and apply .nostos/direct.sql",
        ));
        return Ok(out);
    }

    let rls: bool = client
        .query_one(
            "select relrowsecurity from pg_class where oid = 'nostos.changes'::regclass",
            &[],
        )
        .await?
        .get(0);
    out.push(Check::new(
        rls,
        "row level security enabled on nostos.changes",
    ));

    let policies: Vec<(String, String)> = client
        .query(
            "select policyname, cmd from pg_policies \
             where schemaname = 'nostos' and tablename = 'changes'",
            &[],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let read_only = !policies.is_empty() && policies.iter().all(|(_, cmd)| cmd == "SELECT");
    out.push(Check::new(
        read_only,
        format!(
            "nostos.changes has read-only policies ({})",
            if policies.is_empty() {
                "none found \u{2014} every device would see an empty log".to_string()
            } else {
                policies
                    .iter()
                    .map(|(n, c)| format!("{n}:{c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    ));

    // `nostos_snapshot` is checked alongside the other two because without it a
    // 410 is terminal: the device is told to re-snapshot and has nothing to
    // call. A deploy that predates it looks healthy right up to the first
    // device that comes back after the retention window.
    for (proname, args) in [
        ("nostos_pull", "xid8, int"),
        ("nostos_snapshot", ""),
        ("nostos_increment", "text, text, text, numeric"),
    ] {
        // `proretset` rather than a grep of the definition: `pg_get_functiondef`
        // renders the header in UPPERCASE (`RETURNS jsonb`), so matching the
        // generator's lowercase source text against it never hits — a check
        // that fails a deploy it just approved. The body greps below match
        // body text, which comes back verbatim.
        let def: Option<(String, bool)> = client
            .query_opt(
                "select pg_get_functiondef(p.oid), p.proretset from pg_proc p \
                 join pg_namespace n on n.oid = p.pronamespace \
                 where n.nspname = 'public' and p.proname = $1",
                &[&proname],
            )
            .await?
            .map(|r| (r.get(0), r.get(1)));
        let Some((def, returns_set)) = def else {
            out.push(Check::new(false, format!("public.{proname} exists")));
            continue;
        };
        out.push(Check::new(true, format!("public.{proname} exists")));

        // The other bug no client can detect. PostgREST caps a set-returning
        // RPC at `db-max-rows` and reports the cut in a header nobody reads,
        // on a 200 — so a read simply arrives short, and the device then
        // stores a horizon past rows it never saw. A scalar result is one row
        // at any size, so the cap cannot reach it.
        if matches!(proname, "nostos_pull" | "nostos_snapshot") {
            let scalar = !returns_set;
            out.push(Check::new(
                scalar,
                if scalar {
                    format!("{proname} returns one jsonb value, so `db-max-rows` cannot cut it")
                } else {
                    format!(
                        "{proname} returns a SET \u{2014} PostgREST truncates it at \
                         `db-max-rows` (1000 by default) on a 200 with no error, and the \
                         device stores a horizon past rows it never received. \
                         Regenerate with `nostos link --mode direct`."
                    )
                },
            ));
        }

        if proname == "nostos_pull" {
            // See the doc comment: this is the one bug no client can detect.
            let by_txn = def.contains("select distinct") && def.contains("greatest(max_txns, 2)");
            out.push(Check::new(
                by_txn,
                if by_txn {
                    "nostos_pull pages by transaction".to_string()
                } else {
                    "nostos_pull does NOT page by transaction \u{2014} it can hand a device \
                     half a transaction, which applies atomically and looks correct. \
                     Regenerate with `nostos link --mode direct`."
                        .to_string()
                },
            ));
            let guards_retention = def.contains("PT410");
            out.push(Check::new(
                guards_retention,
                if guards_retention {
                    "nostos_pull reports a pruned horizon as 410 rather than a short answer"
                        .to_string()
                } else {
                    "nostos_pull does NOT guard the retention window \u{2014} a device that was \
                     offline too long resumes into a gap and never receives the missing rows"
                        .to_string()
                },
            ));
            let inclusive = def.contains("c.xid >= since");
            out.push(Check::new(
                inclusive,
                if inclusive {
                    "nostos_pull resumes inclusively (`xid >= since`)".to_string()
                } else {
                    "nostos_pull uses an exclusive lower bound \u{2014} it silently drops the \
                     transaction sitting exactly on the horizon"
                        .to_string()
                },
            ));
        }

        let granted: bool = client
            .query_one(
                &format!(
                    "select has_function_privilege('authenticated', 'public.{proname}({args})', 'execute')"
                ),
                &[],
            )
            .await?
            .get(0);
        out.push(Check::new(
            granted,
            format!("authenticated may execute public.{proname}"),
        ));

        let anon_exists: bool = client
            .query_one(
                "select exists (select from pg_roles where rolname = 'anon')",
                &[],
            )
            .await?
            .get(0);
        if anon_exists {
            let anon: bool = client
                .query_one(
                    &format!(
                        "select has_function_privilege('anon', 'public.{proname}({args})', 'execute')"
                    ),
                    &[],
                )
                .await?
                .get(0);
            out.push(Check::new(
                !anon,
                format!(
                    "anon may NOT execute public.{proname}{}",
                    if anon {
                        " \u{2014} the publishable key can read the whole change log"
                    } else {
                        ""
                    }
                ),
            ));
        }
    }

    let can_select: bool = client
        .query_one(
            "select has_table_privilege('authenticated', 'nostos.changes', 'select')",
            &[],
        )
        .await?
        .get(0);
    let can_write: bool = client
        .query_one(
            "select has_table_privilege('authenticated', 'nostos.changes', 'insert') \
                 or has_table_privilege('authenticated', 'nostos.changes', 'update') \
                 or has_table_privilege('authenticated', 'nostos.changes', 'delete')",
            &[],
        )
        .await?
        .get(0);
    out.push(Check::new(
        can_select,
        "authenticated may read nostos.changes",
    ));
    out.push(Check::new(
        !can_write,
        "authenticated may NOT write nostos.changes (history is append-only, by the trigger)",
    ));

    for t in tables {
        let present: bool = client
            .query_one(
                "select exists (select from pg_trigger g \
                   join pg_class c on c.oid = g.tgrelid \
                   where c.relname = $1 and g.tgname = $2 and not g.tgisinternal)",
                &[&t.table, &format!("nostos_log_{}", t.table)],
            )
            .await?
            .get(0);
        out.push(Check::new(
            present,
            format!("public.{} has its change-log trigger", t.table),
        ));
    }

    // The anti-feedback assertion. A change-log trigger on pg_net's queue turns
    // every push attempt into a change row, which fires another push; one on
    // nostos.device_presence turns a liveness ping into fan-out for every device
    // in the scope. Neither is reachable through `nostos link` — it refuses the
    // names — but a hand-applied migration can do it, and the symptom is an
    // unexplained write storm rather than an error.
    let feedback: Vec<String> = client
        .query(
            "select n.nspname || '.' || c.relname \
             from pg_trigger g join pg_class c on c.oid = g.tgrelid \
             join pg_namespace n on n.oid = c.relnamespace \
             where not g.tgisinternal and g.tgname like 'nostos\\_log\\_%' \
               and n.nspname in ('net', 'nostos')",
            &[],
        )
        .await?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    out.push(Check::new(
        feedback.is_empty(),
        if feedback.is_empty() {
            "neither pg_net nor nostos's own tables are instrumented".to_string()
        } else {
            format!(
                "change-log triggers on {feedback:?} \u{2014} these feed the machinery back \
                 into itself (a push logging a change that fires another push). Drop them."
            )
        },
    ));

    let ring: bool = client
        .query_one(
            "select exists (select from pg_trigger \
             where tgname = 'nostos_changes_ring' and not tgisinternal)",
            &[],
        )
        .await?
        .get(0);
    out.push(Check::new(
        ring,
        "the broadcast doorbell trigger is installed",
    ));

    let realtime_policy: bool = client
        .query_one(
            "select exists (select from pg_policies \
             where schemaname = 'realtime' and tablename = 'messages' \
               and policyname = 'nostos_ring_read')",
            &[],
        )
        .await?
        .get(0);
    out.push(Check::new(
        realtime_policy,
        "the Realtime read policy exists on realtime.messages",
    ));
    out.push(Check::note(
        "Realtime Authorization is only ENFORCED with \"Allow public access\" OFF in the \
         project's Realtime settings \u{2014} SQL cannot see that switch, check it in the \
         dashboard",
    ));

    let (rows, bytes, oldest): (i64, String, Option<f64>) = {
        let r = client
            .query_one(
                "select count(*)::bigint, \
                        pg_size_pretty(pg_total_relation_size('nostos.changes')), \
                        extract(epoch from now() - min(logged_at))::float8 \
                 from nostos.changes",
                &[],
            )
            .await?;
        (r.get(0), r.get(1), r.get(2))
    };
    out.push(Check::note(format!(
        "change log: {rows} rows, {bytes}, oldest {} \u{2014} prune with \
         `select nostos.prune()`",
        oldest.map_or_else(|| "n/a".to_string(), |s| format!("{:.0}h old", s / 3600.0))
    )));

    let push_installed: bool = client
        .query_one("select to_regclass('nostos.push_config') is not null", &[])
        .await?
        .get(0);
    if push_installed {
        let (endpoint, has_secret): (Option<String>, bool) = {
            let r = client
                .query_one(
                    "select endpoint, coalesce(secret, '') <> '' from nostos.push_config \
                     where id = 1",
                    &[],
                )
                .await?;
            (r.get(0), r.get(1))
        };
        out.push(Check::new(
            endpoint.as_deref().is_some_and(|e| !e.is_empty()),
            format!(
                "push endpoint configured ({})",
                endpoint.as_deref().unwrap_or("unset")
            ),
        ));
        out.push(Check::new(
            has_secret,
            "nostos.push_config.secret is set \u{2014} without it the Edge Function cannot tell              a real doorbell from anyone who found the URL",
        ));
        let pg_net: bool = client
            .query_one("select to_regproc('net.http_post') is not null", &[])
            .await?
            .get(0);
        out.push(Check::new(
            pg_net,
            if pg_net {
                "pg_net is installed".to_string()
            } else {
                "pg_net is NOT installed \u{2014} the wake trigger skips silently, so a killed                  app never learns there is anything to sync. `create extension pg_net;`"
                    .to_string()
            },
        ));
        // The push RPCs are `security definer`, so a stray EXECUTE grant is
        // not defence in depth -- it bypasses every table grant and policy.
        // Supabase hands `anon` EXECUTE on new public functions by default
        // privileges, and `revoke ... from public` does not take it away, so
        // this really does fire on a project that was set up by hand.
        for (proname, args) in [
            ("nostos_register_push_token", "text, text"),
            ("nostos_deregister_push_token", "text"),
            ("nostos_heartbeat", "text"),
            ("nostos_push_targets", "text"),
        ] {
            let signature = format!("public.{proname}({args})");
            let leaked: bool = client
                .query_one(
                    "select coalesce((select has_function_privilege('anon', $1, 'execute') \
                     from pg_roles where rolname = 'anon'), false)",
                    &[&signature],
                )
                .await?
                .get(0);
            if leaked {
                out.push(Check::new(
                    false,
                    format!(
                        "anon may execute {signature} \u{2014} it is security definer, so \
                         this is a hole, not defence in depth. Re-run `nostos link --mode \
                         direct --push` and re-apply."
                    ),
                ));
            }
        }

        // The check that catches a silently dead push path: the Edge Function
        // runs with the service role and must reach `nostos.push_tokens`, but
        // `nostos` is not an exposed schema. A function that reads the table
        // through the Data API gets `Invalid schema: nostos` and drops every
        // notification with no error anywhere the operator will look.
        let (targets_rpc, service_may_call): (bool, bool) = {
            let r = client
                .query_one(
                    "select to_regprocedure('public.nostos_push_targets(text)') is not null, \
                            coalesce(has_function_privilege('service_role', \
                              'public.nostos_push_targets(text)', 'execute'), false)",
                    &[],
                )
                .await?;
            (r.get(0), r.get(1))
        };
        out.push(Check::new(
            targets_rpc && service_may_call,
            if targets_rpc {
                "the Edge Function can read the token registry (service_role may execute \
                 public.nostos_push_targets)"
                    .to_string()
            } else {
                "public.nostos_push_targets is MISSING \u{2014} the Edge Function would have \
                 to read the unexposed `nostos` schema, which fails with `Invalid schema: \
                 nostos` and drops every notification. Re-run `nostos link --mode direct \
                 --push`."
                    .to_string()
            },
        ));

        let (tokens, awake): (i64, i64) = {
            let r = client
                .query_one(
                    "select (select count(*)::bigint from nostos.push_tokens), \
                            (select count(*)::bigint from nostos.device_presence \
                              where last_seen > now() - (select presence_window \
                                                           from nostos.push_config where id = 1))",
                    &[],
                )
                .await?;
            (r.get(0), r.get(1))
        };
        out.push(Check::note(format!(
            "push: {tokens} registered token(s), {awake} device(s) currently counted as awake"
        )));
    }

    let withheld: i64 = client
        .query_one(
            "select count(*)::bigint from nostos.changes \
             where xid >= pg_snapshot_xmin(pg_current_snapshot())",
            &[],
        )
        .await?
        .get(0);
    let oldest_txn: Option<f64> = client
        .query_one(
            "select max(extract(epoch from now() - xact_start))::float8 from pg_stat_activity \
             where xact_start is not null and backend_xid is not null",
            &[],
        )
        .await?
        .get(0);
    let lag = oldest_txn.unwrap_or(0.0);
    out.push(Check::new(
        lag < 30.0,
        format!(
            "horizon lag: {withheld} row(s) withheld behind the oldest open write \
             transaction ({lag:.0}s). The horizon cannot pass an in-flight write, so a \
             long transaction delays every device by its own duration."
        ),
    ));

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_domain::{HandRule, StreamRule, TableRule, RULES_VERSION};

    fn toggles(entries: &[(&str, bool, Option<&str>)]) -> SyncRules {
        SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: entries
                .iter()
                .map(|(t, sync, scope)| TableRule {
                    table: (*t).to_string(),
                    sync: *sync,
                    scope: scope.map(ToString::to_string),
                })
                .collect(),
            hand: vec![],
            streams: vec![],
        }
    }

    fn err(rules: &SyncRules, public: &[&str]) -> String {
        let public: Vec<String> = public.iter().map(ToString::to_string).collect();
        plan(rules, &public)
            .expect_err("expected a refusal")
            .to_string()
    }

    #[test]
    fn a_claim_scope_is_the_one_expressible_shape() {
        let rules = toggles(&[
            ("tasks", true, Some("org_id = claims.org_id")),
            ("skipped", false, Some("org_id = claims.org_id")),
        ]);
        let plan = plan(&rules, &[]).expect("plannable");
        assert_eq!(
            plan,
            vec![DirectTable {
                table: "tasks".to_string(),
                scoping: Scoping::Claim {
                    column: "org_id".to_string(),
                    claim: "org_id".to_string(),
                },
            }],
            "a table toggled off must not get a trigger"
        );
    }

    /// The whole point of step 6: every shape the single scope column cannot
    /// carry is refused BY NAME, rather than generating a policy that quietly
    /// shows the wrong rows. Each case names why it is not expressible.
    #[test]
    fn every_inexpressible_scope_is_refused_by_name() {
        for (scope, needle) in [
            ("org_id = claims.org AND status = 'open'", "AND-composes"),
            ("priority > claims.min", "only `=`"),
            ("status = 'open'", "literal"),
            ("org_id = claims.org OR true", "cannot be expressed"),
        ] {
            let rules = toggles(&[("tasks", true, Some(scope))]);
            let message = err(&rules, &[]);
            assert!(
                message.contains("tasks") && message.contains(needle),
                "scope `{scope}` should be refused mentioning `{needle}`, got: {message}"
            );
        }
    }

    #[test]
    fn an_unscoped_table_is_refused_until_it_is_declared_public() {
        let rules = toggles(&[("countries", true, None)]);
        let message = err(&rules, &[]);
        assert!(
            message.contains("--public countries"),
            "the refusal must name the opt-in, got: {message}"
        );

        let plan = plan(&rules, &["countries".to_string()]).expect("plannable once declared");
        assert_eq!(plan[0].scoping, Scoping::Public);
    }

    #[test]
    fn public_and_scoped_is_a_contradiction_and_a_typo_is_caught() {
        let both = toggles(&[("tasks", true, Some("org_id = claims.org_id"))]);
        assert!(err(&both, &["tasks"]).contains("pick one"));

        let typo = toggles(&[("tasks", true, Some("org_id = claims.org_id"))]);
        assert!(err(&typo, &["taks"]).contains("not synced"));
    }

    #[test]
    fn streams_and_an_empty_table_set_are_refused() {
        let mut streamed = toggles(&[("tasks", true, Some("org_id = claims.org_id"))]);
        streamed.streams = vec![StreamRule {
            name: "mine".to_string(),
            table: "tasks".to_string(),
            template: "owner_id = :owner".to_string(),
        }];
        assert!(err(&streamed, &[]).contains("server-mode only"));

        let none = toggles(&[("tasks", false, None)]);
        assert!(err(&none, &[]).contains("nostos rules init"));
    }

    #[test]
    fn hand_mode_rules_are_read_from_the_hand_section() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Hand,
            tables: vec![TableRule {
                table: "ignored".to_string(),
                sync: true,
                scope: Some("org_id = claims.org_id".to_string()),
            }],
            hand: vec![HandRule {
                table: "tasks".to_string(),
                scope: Some("owner_id = claims.sub".to_string()),
            }],
            streams: vec![],
        };
        let plan = plan(&rules, &[]).expect("plannable");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].table, "tasks");
    }

    #[test]
    fn an_exotic_identifier_is_refused_rather_than_quoted() {
        let rules = toggles(&[("Tasks", true, Some("org_id = claims.org_id"))]);
        assert!(err(&rules, &[]).contains("plain lowercase SQL identifier"));
    }

    fn sample_sql() -> String {
        render(
            &[
                DirectTable {
                    table: "tasks".to_string(),
                    scoping: Scoping::Claim {
                        column: "owner_id".to_string(),
                        claim: "sub".to_string(),
                    },
                },
                DirectTable {
                    table: "projects".to_string(),
                    scoping: Scoping::Claim {
                        column: "org_id".to_string(),
                        claim: "org_id".to_string(),
                    },
                },
                DirectTable {
                    table: "countries".to_string(),
                    scoping: Scoping::Public,
                },
            ],
            DEFAULT_RETENTION,
        )
    }

    /// ADR-0048: a pre-rename project is renamed before anything is created,
    /// or the `create ... if not exists` below would build a second, empty log
    /// beside the renamed one.
    #[test]
    fn a_pre_rename_project_is_renamed_before_the_schema_is_created() {
        let sql = sample_sql();
        let rename = sql
            .find("alter schema %I rename to nostos")
            .expect("rename block");
        let create = sql
            .find("create schema if not exists nostos")
            .expect("create");
        assert!(rename < create, "the rename must run first");
    }

    /// Pins the contract `nostos_core::pull` and `nostos_client::postgrest`
    /// already depend on. A rename here is a wire break, so it should fail a
    /// test rather than a device.
    #[test]
    fn the_generated_sql_matches_the_client_contract() {
        let sql = sample_sql();
        for needle in [
            // The client posts here, with these argument names.
            "function public.nostos_pull(since xid8, max_txns int default 200)",
            "function public.nostos_increment(",
            "p_table text, p_pk text, p_field text, p_delta numeric",
            // The keys `PullRow` deserializes, now carried by `to_jsonb` over
            // the column names rather than by a table signature.
            r#"select h.horizon, c.seq, c.xid, c.table_name, c.pk, c.op, c."row""#,
            // One jsonb value, never a set: PostgREST truncates a set at
            // `db-max-rows` and says so only in a content-range header.
            "returns jsonb",
            "coalesce(jsonb_agg(to_jsonb(r) order by r.xid, r.seq), '[]'::jsonb)",
            "coalesce(jsonb_agg(to_jsonb(snap)), '[]'::jsonb)",
            // Inclusive lower bound + transaction paging: the livelock fix.
            "where c.xid >= since and c.xid < h.horizon",
            "limit greatest(max_txns, 2)",
            // The topic `nostos_client::doorbell` joins.
            "'nostos:' || coalesce(new.scope, 'unscoped')",
        ] {
            assert!(sql.contains(needle), "generated SQL is missing: {needle}");
        }
    }

    #[test]
    fn each_table_gets_a_trigger_carrying_its_own_scope_binding() {
        let sql = sample_sql();
        assert!(sql.contains("execute function nostos.log_change('owner_id', 'sub');"));
        assert!(sql.contains("execute function nostos.log_change('org_id', 'org_id');"));
        // A public table passes no arguments, so the trigger stamps the
        // literal public scope instead of reading a column.
        assert!(sql.contains("execute function nostos.log_change();"));
        for t in ["tasks", "projects", "countries"] {
            assert!(sql.contains(&format!(
                "drop trigger if exists nostos_log_{t} on public.{t};"
            )));
        }
    }

    /// Two claims must never collide in the one shared column, and the anon
    /// A 410 the device cannot act on is a device bricked by a long holiday,
    /// so the snapshot is part of the retention design, not a follow-up.
    #[test]
    fn the_snapshot_is_one_statement_and_announces_every_table() {
        let sql = sample_sql();
        let body = sql
            .split("create or replace function public.nostos_snapshot()")
            .nth(1)
            .expect("nostos_snapshot is generated")
            .split("$fn$;")
            .next()
            .expect("the function body terminates");
        // One `with h as (...)` feeding every branch: the snapshot has to be as
        // cross-table consistent as a pull, which means one statement.
        assert_eq!(
            body.matches("pg_current_snapshot()").count(),
            1,
            "more than one snapshot would make the tables mutually inconsistent"
        );
        assert!(!body.contains(';'), "the body must be a single statement");
        for table in ["tasks", "projects", "countries"] {
            assert!(
                body.contains(&format!("select h.horizon, '{table}'::text, null::text")),
                "{table} needs a header row, or an empty {table} is \
                 indistinguishable from one the snapshot forgot"
            );
            assert!(body.contains(&format!("from public.{table} r, h")));
        }
        // Same authority as the pull, or the two are not interchangeable.
        assert!(body.contains("security invoker"));
        assert!(sql.contains(
            "revoke all on function public.nostos_snapshot() from public, anon, authenticated;"
        ));
    }

    /// key must never reach the log.
    #[test]
    fn scopes_are_namespaced_and_the_functions_are_revoked_from_public() {
        let sql = sample_sql();
        assert!(sql.contains("'org_id:' || (auth.jwt() ->> 'org_id')"));
        assert!(sql.contains("'sub:' || (auth.jwt() ->> 'sub')"));
        assert!(
            sql.contains("'public'\n  ], null);"),
            "public scope element"
        );
        // Naming the roles is load-bearing, not stylistic: Supabase's default
        // privileges grant EXECUTE to `anon` BY NAME, and revoking from the
        // PUBLIC pseudo-role leaves that grant standing.
        assert!(sql.contains(
            "revoke all on function public.nostos_pull(xid8, int) from public, anon, authenticated;"
        ));
        for line in sql
            .lines()
            .filter(|l| l.starts_with("revoke all on function"))
        {
            let whole = line.trim_end();
            assert!(
                whole.contains("from public, anon, authenticated") || whole.ends_with(')'),
                "a revoke that names only PUBLIC leaves anon holding EXECUTE: {whole}"
            );
        }
        assert!(sql
            .contains("grant execute on function public.nostos_pull(xid8, int) to authenticated;"));
        assert!(
            !sql.contains("to anon"),
            "the anon role must never be granted the change log"
        );
        // Devices read history; only the security-definer trigger writes it.
        assert!(sql.contains("grant select on nostos.changes to authenticated;"));
        assert!(!sql.contains("grant insert on nostos.changes"));
    }

    #[test]
    fn the_increment_allow_list_is_the_synced_table_set() {
        let sql = sample_sql();
        assert!(sql.contains("if p_table not in ('tasks', 'projects', 'countries') then"));
    }

    #[test]
    fn push_is_opt_in_and_never_instruments_its_own_tables() {
        let plain = sample_sql();
        assert!(
            !plain.contains("nostos.push_tokens"),
            "no --push means no push objects at all"
        );

        let with_push = render_with_push(
            &[DirectTable {
                table: "tasks".to_string(),
                scoping: Scoping::Claim {
                    column: "owner_id".to_string(),
                    claim: "sub".to_string(),
                },
            }],
            DEFAULT_RETENTION,
            Some(&PushConfig {
                endpoint: "https://ref.functions.supabase.co/nostos-push".to_string(),
                ..PushConfig::default()
            }),
        );
        for needle in [
            "function public.nostos_register_push_token(p_platform text, p_token text)",
            "function public.nostos_deregister_push_token(p_token text)",
            "function public.nostos_heartbeat(p_device_id text)",
            // The Edge Function's only way into the registry: `nostos` is not an
            // exposed schema, so a direct table read fails with `Invalid
            // schema: nostos` and drops every notification silently.
            "function public.nostos_push_targets(p_scope text)",
            "grant execute on function public.nostos_push_targets(text) to service_role;",
            "https://ref.functions.supabase.co/nostos-push",
            // The atomic per-scope debounce, which is also the pg_net rate guard.
            "on conflict (scope) do update set last_push_at = now()",
            "pg_try_advisory_xact_lock(hashtext('nostos:push:' || new.scope))",
            "where nostos.push_cooldown.last_push_at < now() - v_cfg.cooldown",
            // Skipping when pg_net is absent beats failing every write.
            "if to_regproc('net.http_post') is null then",
        ] {
            assert!(with_push.contains(needle), "push SQL is missing: {needle}");
        }
        // The trigger is attached to public tables only — never to nostos's own.
        assert!(!with_push.contains("execute function nostos.log_change('scope'"));
        assert_eq!(
            with_push
                .matches("after insert or update or delete on")
                .count(),
            1,
            "exactly one synced table is instrumented"
        );
    }

    #[test]
    fn visible_specs_parse_like_nostos_push_tables_and_render_quoted() {
        let action = parse_visible(
            "order_events:action@/history/{id}:order_status:Atlet: order:Your order's {status}",
        )
        .unwrap();
        assert_eq!(
            action,
            PushTemplate {
                table: "order_events".to_string(),
                title: "Atlet".to_string(),
                // The body is the greedy remainder, colons and all.
                body: "order:Your order's {status}".to_string(),
                category: Some("order_status".to_string()),
                route: Some("/history/{id}".to_string()),
                options: nostos_infra::push::PushOptions::new(),
            }
        );
        let plain = parse_visible("orders:visible:New order:Order {id} placed").unwrap();
        assert_eq!((&plain.category, &plain.route), (&None, &None));

        for bad in [
            "orders",
            "orders:silent",
            "orders:visible:title only",
            "orders:action:order_status:title only",
            "Orders:visible:t:b",
            "orders:action:Order-Status:t:b",
            "orders:visible@history:t:b",
            "orders:visible[level=loud]:t:b",
        ] {
            assert!(parse_visible(bad).is_err(), "{bad:?} must be refused");
        }

        let sql = templates_sql(&[action]);
        assert!(
            sql.starts_with("delete from nostos.push_templates;\n"),
            "{sql}"
        );
        assert!(sql.contains("'order:Your order''s {status}', 'order_status', '/history/{id}'"));
        assert!(templates_sql(&[plain]).contains(", null, null, '{}');"));

        // The same `[k=v,…]` group as NOSTOS_PUSH_TABLES, stored as jsonb.
        let rich = parse_visible(
            "order_events:visible@/o/{id}[image=https://cdn.example/{status}.png]:T:B",
        )
        .unwrap();
        assert_eq!(rich.route.as_deref(), Some("/o/{id}"));
        assert!(templates_sql(&[rich])
            .contains(r#"'/o/{id}', '{"image":"https://cdn.example/{status}.png"}');"#));
    }

    #[test]
    fn direct_modes_own_tables_are_refused_as_sync_targets() {
        for reserved in ["changes", "device_presence", "push_tokens"] {
            let rules = toggles(&[(reserved, true, Some("org_id = claims.org_id"))]);
            let message = err(&rules, &[]);
            assert!(
                message.contains("machinery back into itself")
                    || message.contains("feed the machinery"),
                "`{reserved}` must be refused: {message}"
            );
        }
    }

    #[test]
    fn rendering_is_deterministic_and_re_runnable() {
        assert_eq!(sample_sql(), sample_sql());
        let sql = sample_sql();
        assert!(sql.starts_with("-- Generated by `nostos link --mode direct`"));
        assert!(sql.contains("\nbegin;\n") && sql.trim_end().ends_with("commit;"));
        // Nothing may fail on a second apply.
        assert!(!sql.contains("create table nostos.changes ("));
        // current_scopes, log_change, nostos_pull, nostos_snapshot,
        // nostos_increment, ring, prune. All `or replace`, so re-applying the
        // file is a no-op rather than a duplicate-object error.
        assert_eq!(sql.matches("create or replace function").count(), 7);
        assert_eq!(
            sql.matches("drop policy if exists").count(),
            sql.matches("create policy").count()
        );
    }
}
