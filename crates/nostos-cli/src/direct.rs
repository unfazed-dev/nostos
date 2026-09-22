//! Direct-mode SQL generation — the server-side half of
//! `docs/plans/direct-mode-sync-protocol.md` (step 6).
//!
//! Direct mode has no Nostos server: the device pulls from the client's own
//! Postgres through PostgREST and is woken by a Realtime broadcast. Everything
//! that makes that safe lives in the database, so `nostos link --mode direct`
//! emits it as one re-runnable SQL file: the `cairn.changes` log, a per-table
//! trigger that appends to it inside the writing transaction, `cairn_pull`,
//! `cairn_increment`, the broadcast doorbell, RLS and the grants.
//!
//! ## Why the refusal is the interesting part
//!
//! The log holds row images from many tables, so **one** policy on
//! `cairn.changes` has to say what N per-table policies say. The trigger
//! stamps a single `scope` text column and RLS compares it to the caller's
//! claims — which only works when a table's rule is exactly
//! `<column> = claims.<field>`. Anything else (a literal term, an inequality,
//! two AND-ed terms, a join) cannot be collapsed into one column, so
//! [`plan`] refuses it by name instead of generating a policy that silently
//! shows the wrong rows. That refusal is cost #2 in the plan, made executable.
//!
//! ## Why `cairn_pull` lives in `public`, not in `nostos`
//!
//! Supabase's default exposed schemas are `public, graphql_public`; a third
//! schema is reachable only with a `Content-Profile` header, and only after an
//! operator ticks it into "Exposed schemas" in the dashboard. Putting the two
//! entry points in `public` under a `nostos_` prefix removes both: the client
//! posts to `/rest/v1/rpc/cairn_pull` with no extra header, and the log table
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

/// How long `cairn.prune()` keeps change rows by default. A device offline
/// longer than this has to re-snapshot (plan cost #4).
pub const DEFAULT_RETENTION: &str = "7 days";

/// `.nostos/direct.sql` — the generated file, relative to `.nostos/`.
pub const OUTPUT_FILE: &str = "direct.sql";

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
    increment_fn(&mut s, tables);
    doorbell(&mut s);
    policies_and_grants(&mut s);
    prune(&mut s, retention);
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
    s.push_str("\nbegin;\n\ncreate schema if not exists cairn;\n");
}

fn log_table(s: &mut String) {
    s.push_str(
        r#"
-- The transactional outbox. The trigger below appends to it INSIDE the writing
-- transaction, so a data change and its change record commit together or not at
-- all. `xid` is what makes a cross-table transaction reassemblable on the
-- device; `seq` only orders rows within one.
create table if not exists cairn.changes (
  seq        bigserial primary key,
  xid        xid8        not null default pg_current_xact_id(),
  table_name text        not null,
  pk         text        not null,
  op         text        not null,
  "row"      jsonb,
  scope      text,
  logged_at  timestamptz not null default now()
);
create index if not exists changes_xid_seq_idx    on cairn.changes (xid, seq);
create index if not exists changes_logged_at_idx  on cairn.changes (logged_at);
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
create or replace function cairn.current_scopes()
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
create or replace function cairn.log_change() returns trigger
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
    insert into cairn.changes (table_name, pk, op, "row", scope)
    values (tg_table_name, v_old ->> '{PK_COLUMN}', 'delete', null, v_old_scope);
  end if;

  insert into cairn.changes (table_name, pk, op, "row", scope)
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
            "drop trigger if exists cairn_log_{name} on public.{name};\n\
             create trigger cairn_log_{name}\n  \
             after insert or update or delete on public.{name}\n  \
             for each row execute function cairn.log_change{args};\n\n"
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
-- security invoker: RLS on cairn.changes applies as the calling device.
create or replace function public.cairn_pull(since xid8, max_txns int default 200)
returns table (horizon xid8, seq bigint, xid xid8,
               table_name text, pk text, op text, "row" jsonb)
language sql stable security invoker set search_path = '' as $fn$
  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon),
  page as (
    select distinct c.xid
    from cairn.changes c, h
    where c.xid >= since and c.xid < h.horizon
    order by c.xid
    limit greatest(max_txns, 2)
  )
  select h.horizon, c.seq, c.xid, c.table_name, c.pk, c.op, c."row"
  from cairn.changes c
  join page p on p.xid = c.xid
  cross join h
  order by c.xid, c.seq;
$fn$;
"#,
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
create or replace function public.cairn_increment(
  p_table text, p_pk text, p_field text, p_delta numeric)
returns void language plpgsql security invoker set search_path = '' as $fn$
declare
  v_pk_type text;
begin
  if p_table not in ({allowed}) then
    raise exception 'cairn_increment: table % is not synced', p_table
      using errcode = '42501';
  end if;

  select a.atttypid::regtype::text into v_pk_type
  from pg_catalog.pg_attribute a
  where a.attrelid = format('public.%I', p_table)::regclass
    and a.attname = '{PK_COLUMN}'
    and a.attnum > 0;
  if v_pk_type is null then
    raise exception 'cairn_increment: public.% has no {PK_COLUMN} column', p_table
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
create or replace function cairn.ring() returns trigger
language plpgsql security definer set search_path = '' as $fn$
begin
  perform realtime.send(
    '{}'::jsonb,                                  -- payload: deliberately empty
    'cairn_ring',                                 -- event
    'cairn:' || coalesce(new.scope, 'unscoped'),  -- topic (channel joins as realtime:<topic>)
    true                                          -- private: RLS below authorizes it
  );
  return null;
end;
$fn$;

drop trigger if exists cairn_changes_ring on cairn.changes;
create trigger cairn_changes_ring
  after insert on cairn.changes
  for each row execute function cairn.ring();
"#,
    );
}

fn policies_and_grants(s: &mut String) {
    s.push_str(
        r#"
-- RLS is the ONLY thing authorizing a direct-mode read. That is the security
-- argument, not a caveat: a forbidden row is refused by Postgres rather than by
-- a service the developer has to trust.
alter table cairn.changes enable row level security;

drop policy if exists cairn_changes_read on cairn.changes;
create policy cairn_changes_read on cairn.changes
  for select to authenticated
  using (scope = any (cairn.current_scopes()));

-- No insert/update/delete policy exists, and none should: cairn.log_change is
-- security definer, so the trigger writes history and nobody else can.

-- The private Realtime channel. Broadcast-from-database requires Realtime
-- Authorization, which is a policy on realtime.messages — and it is only
-- ENFORCED once "Allow public access" is off in the project's Realtime
-- settings. `nostos doctor --mode direct` checks that; SQL cannot.
drop policy if exists cairn_ring_read on realtime.messages;
create policy cairn_ring_read on realtime.messages
  for select to authenticated
  using (
    realtime.messages.extension = 'broadcast'
    and (select realtime.topic()) like 'cairn:%'
    and substring((select realtime.topic()) from 7) = any (cairn.current_scopes())
  );

grant usage on schema cairn to authenticated;
grant select on cairn.changes to authenticated;
grant execute on function cairn.current_scopes() to authenticated;

-- Postgres grants EXECUTE to PUBLIC on every new function, which would hand the
-- whole change log to the anon key. Revoke first, then grant narrowly.
revoke all on function public.cairn_pull(xid8, int) from public;
revoke all on function public.cairn_increment(text, text, text, numeric) from public;
grant execute on function public.cairn_pull(xid8, int) to authenticated;
grant execute on function public.cairn_increment(text, text, text, numeric) to authenticated;
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
--   select cron.schedule('nostos-prune', '0 * * * *', $$select cairn.prune()$$);
create or replace function cairn.prune(retain interval default interval '{retention}')
returns bigint language plpgsql security definer set search_path = '' as $fn$
declare n bigint;
begin
  delete from cairn.changes where logged_at < now() - retain;
  get diagnostics n = row_count;
  return n;
end;
$fn$;

commit;
"
    );
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

    /// Pins the contract `nostos_core::pull` and `nostos_client::postgrest`
    /// already depend on. A rename here is a wire break, so it should fail a
    /// test rather than a device.
    #[test]
    fn the_generated_sql_matches_the_client_contract() {
        let sql = sample_sql();
        for needle in [
            // The client posts here, with these argument names.
            "function public.cairn_pull(since xid8, max_txns int default 200)",
            "function public.cairn_increment(",
            "p_table text, p_pk text, p_field text, p_delta numeric",
            // The columns `PullRow` deserializes.
            r"returns table (horizon xid8, seq bigint, xid xid8,",
            r#"table_name text, pk text, op text, "row" jsonb)"#,
            // Inclusive lower bound + transaction paging: the livelock fix.
            "where c.xid >= since and c.xid < h.horizon",
            "limit greatest(max_txns, 2)",
            "order by c.xid, c.seq",
            // The topic `nostos_client::doorbell` joins.
            "'cairn:' || coalesce(new.scope, 'unscoped')",
        ] {
            assert!(sql.contains(needle), "generated SQL is missing: {needle}");
        }
    }

    #[test]
    fn each_table_gets_a_trigger_carrying_its_own_scope_binding() {
        let sql = sample_sql();
        assert!(sql.contains("execute function cairn.log_change('owner_id', 'sub');"));
        assert!(sql.contains("execute function cairn.log_change('org_id', 'org_id');"));
        // A public table passes no arguments, so the trigger stamps the
        // literal public scope instead of reading a column.
        assert!(sql.contains("execute function cairn.log_change();"));
        for t in ["tasks", "projects", "countries"] {
            assert!(sql.contains(&format!(
                "drop trigger if exists cairn_log_{t} on public.{t};"
            )));
        }
    }

    /// Two claims must never collide in the one shared column, and the anon
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
        assert!(sql.contains("revoke all on function public.cairn_pull(xid8, int) from public;"));
        assert!(sql
            .contains("grant execute on function public.cairn_pull(xid8, int) to authenticated;"));
        assert!(
            !sql.contains("to anon"),
            "the anon role must never be granted the change log"
        );
        // Devices read history; only the security-definer trigger writes it.
        assert!(sql.contains("grant select on cairn.changes to authenticated;"));
        assert!(!sql.contains("grant insert on cairn.changes"));
    }

    #[test]
    fn the_increment_allow_list_is_the_synced_table_set() {
        let sql = sample_sql();
        assert!(sql.contains("if p_table not in ('tasks', 'projects', 'countries') then"));
    }

    #[test]
    fn rendering_is_deterministic_and_re_runnable() {
        assert_eq!(sample_sql(), sample_sql());
        let sql = sample_sql();
        assert!(sql.starts_with("-- Generated by `nostos link --mode direct`"));
        assert!(sql.contains("\nbegin;\n") && sql.trim_end().ends_with("commit;"));
        // Nothing may fail on a second apply.
        assert!(!sql.contains("create table cairn.changes ("));
        assert_eq!(sql.matches("create or replace function").count(), 6);
        assert_eq!(
            sql.matches("drop policy if exists").count(),
            sql.matches("create policy").count()
        );
    }
}
