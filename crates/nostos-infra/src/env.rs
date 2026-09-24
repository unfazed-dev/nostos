//! Process-env reads that survive the project rename (ADR-0046).
//!
//! Every `NOSTOS_*` read in the workspace goes through [`var`] / [`var_os`]
//! (or [`parse`] for clap `env =` attributes; [`fold_legacy`] for a parsed
//! `.env` map).
//! The caller passes the full current name. When it is unset, starts with
//! [`PREFIX`], and [`PREFIX`] differs from [`LEGACY_PREFIX`], the pre-rename
//! name is read instead and a one-time deprecation warning goes to stderr.
//! Before the rename the two prefixes are equal, so the fallback never runs.
//!
//! No `std::env::set_var` anywhere: copying the legacy value into the new
//! name would race every concurrent reader under tokio (and is `unsafe` from
//! edition 2024). `clippy.toml` bans direct `std::env` reads so nothing
//! bypasses this module.

use std::collections::{BTreeMap, BTreeSet};
use std::env::VarError;
use std::ffi::OsString;
use std::sync::{Mutex, PoisonError};

/// Prefix of every env var this workspace reads.
pub const PREFIX: &str = "NOSTOS_";
/// Pre-rename prefix, read as a fallback until 1.0.
pub const LEGACY_PREFIX: &str = "CAIRN_"; // rename:hold — pre-rename env name, read as fallback until 1.0 (decision 10)

/// `name`'s pre-rename spelling: only when it carries `prefix` and the
/// prefixes differ (equal prefixes would read the same var twice).
fn legacy_name(name: &str, prefix: &str, legacy_prefix: &str) -> Option<String> {
    let rest = name.strip_prefix(prefix)?;
    (prefix != legacy_prefix).then(|| format!("{legacy_prefix}{rest}"))
}

/// Pure core: `name` from `source`, else its legacy spelling. The second
/// field names the legacy var when the fallback supplied the value.
fn resolve<T>(
    name: &str,
    prefix: &str,
    legacy_prefix: &str,
    source: impl Fn(&str) -> Option<T>,
) -> Option<(T, Option<String>)> {
    if let Some(value) = source(name) {
        return Some((value, None));
    }
    let legacy = legacy_name(name, prefix, legacy_prefix)?;
    source(&legacy).map(|value| (value, Some(legacy)))
}

/// Warn once per legacy name (env var or config path). `eprintln!`, not
/// `tracing`: clap resolves env before the subscriber exists (`NOSTOS_LOG` is
/// itself an arg), and a once-only warning dropped then would never show.
pub(crate) fn warn_once(legacy: &str, advice: &str) {
    static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let first = WARNED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(legacy.to_owned());
    if first {
        eprintln!("warning: {legacy} is deprecated, {advice} (read as a fallback until 1.0)");
    }
}

/// Drop-in for [`std::env::var_os`] with the legacy fallback.
#[must_use]
#[allow(clippy::disallowed_methods)] // the one sanctioned process-env read
pub fn var_os(name: &str) -> Option<OsString> {
    let (value, legacy) = resolve(name, PREFIX, LEGACY_PREFIX, |n| std::env::var_os(n))?;
    if let Some(legacy) = legacy {
        warn_once(&legacy, &format!("set {name} instead"));
    }
    Some(value)
}

/// Copy each pre-rename key of a whole-map read (a parsed `.env`, an env
/// snapshot) to its current name, unless that name is already set — so
/// plain `map.get("NOSTOS_…")` lookups get the fallback.
pub fn fold_legacy<V: Clone>(vars: &mut BTreeMap<String, V>) {
    fold(vars, PREFIX, LEGACY_PREFIX);
}

fn fold<V: Clone>(vars: &mut BTreeMap<String, V>, prefix: &str, legacy_prefix: &str) {
    if prefix == legacy_prefix {
        return;
    }
    let renamed: Vec<(String, String)> = vars
        .keys()
        .filter_map(|k| {
            Some((
                format!("{prefix}{}", k.strip_prefix(legacy_prefix)?),
                k.clone(),
            ))
        })
        .collect();
    for (name, legacy) in renamed {
        if !vars.contains_key(&name) {
            warn_once(&legacy, &format!("set {name} instead"));
            let value = vars[&legacy].clone();
            vars.insert(name, value);
        }
    }
}

/// Drop-in for [`std::env::var`] with the legacy fallback.
///
/// # Errors
/// [`VarError::NotPresent`] when neither name is set;
/// [`VarError::NotUnicode`] when the value is not UTF-8.
pub fn var(name: &str) -> Result<String, VarError> {
    var_os(name)
        .ok_or(VarError::NotPresent)?
        .into_string()
        .map_err(VarError::NotUnicode)
}

/// Point each `env = "NOSTOS_*"` arg at its legacy var when only that one is
/// set. Swapping the name (not injecting a value) keeps clap's own
/// precedence (flag > env > default), bool parsing and `hide_env_values`.
#[cfg(feature = "clap")]
#[must_use]
pub fn with_legacy_env(cmd: clap::Command) -> clap::Command {
    #[allow(clippy::disallowed_methods)] // presence check only; the read stays in clap
    let is_set = |n: &str| std::env::var_os(n).is_some();
    swap_env(cmd, PREFIX, LEGACY_PREFIX, &is_set)
}

#[cfg(feature = "clap")]
fn swap_env(
    cmd: clap::Command,
    prefix: &str,
    legacy_prefix: &str,
    is_set: &dyn Fn(&str) -> bool,
) -> clap::Command {
    cmd.mut_args(|arg| {
        let Some(name) = arg.get_env().and_then(std::ffi::OsStr::to_str) else {
            return arg;
        };
        match legacy_name(name, prefix, legacy_prefix) {
            Some(legacy) if !is_set(name) && is_set(&legacy) => {
                warn_once(&legacy, &format!("set {name} instead"));
                arg.env(legacy)
            }
            _ => arg,
        }
    })
    .mut_subcommands(|sub| swap_env(sub, prefix, legacy_prefix, is_set))
}

/// Drop-in for [`clap::Parser::parse`] with the legacy env fallback.
#[cfg(feature = "clap")]
#[must_use]
pub fn parse<P: clap::Parser>() -> P {
    let mut matches = with_legacy_env(P::command()).get_matches();
    P::from_arg_matches_mut(&mut matches).unwrap_or_else(|e| e.format(&mut P::command()).exit())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn source(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |n| map.get(n).cloned()
    }

    #[test]
    fn legacy_only_falls_back_and_names_the_legacy_var() {
        let got = resolve("NEW_URL", "NEW_", "OLD_", source(&[("OLD_URL", "old")]));
        assert_eq!(got, Some(("old".to_owned(), Some("OLD_URL".to_owned()))));
    }

    #[test]
    fn primary_only_reads_primary() {
        let got = resolve("NEW_URL", "NEW_", "OLD_", source(&[("NEW_URL", "new")]));
        assert_eq!(got, Some(("new".to_owned(), None)));
    }

    #[test]
    fn both_set_primary_wins() {
        let src = source(&[("NEW_URL", "new"), ("OLD_URL", "old")]);
        assert_eq!(
            resolve("NEW_URL", "NEW_", "OLD_", src),
            Some(("new".to_owned(), None))
        );
    }

    #[test]
    fn neither_set_is_none() {
        assert_eq!(resolve("NEW_URL", "NEW_", "OLD_", source(&[])), None);
    }

    #[test]
    fn unprefixed_name_never_falls_back() {
        let src = source(&[("OLD_URL", "old")]);
        assert_eq!(resolve("RUST_LOG", "NEW_", "OLD_", src), None);
    }

    #[test]
    fn equal_prefixes_are_a_no_op() {
        let calls = std::cell::Cell::new(0);
        let got = resolve("NEW_URL", "NEW_", "NEW_", |_: &str| {
            calls.set(calls.get() + 1);
            None::<String>
        });
        assert_eq!(got, None);
        assert_eq!(calls.get(), 1, "the same var must not be read twice");
    }

    #[test]
    fn fold_copies_legacy_keys_the_primary_lacks() {
        let mut vars: BTreeMap<String, &str> =
            [("OLD_A", "old-a"), ("OLD_B", "old-b"), ("NEW_B", "new-b")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
        fold(&mut vars, "NEW_", "OLD_");
        assert_eq!(vars["NEW_A"], "old-a", "legacy only");
        assert_eq!(vars["NEW_B"], "new-b", "both: primary wins");

        let before = vars.clone();
        fold(&mut vars, "NEW_", "NEW_");
        assert_eq!(vars, before, "equal prefixes: no-op");
    }

    #[test]
    fn legacy_prefix_is_the_pre_rename_name() {
        assert_eq!(LEGACY_PREFIX, "CAIRN_"); // rename:hold — pins the fallback to the names deployments already set
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_env_swaps_to_legacy_only_when_primary_unset() {
        use clap::{Arg, Command};
        let cmd = || {
            Command::new("t")
                .arg(Arg::new("a").long("a").env("NEW_A"))
                .subcommand(Command::new("s").arg(Arg::new("b").long("b").env("NEW_B")))
        };
        let env_of = |c: &Command, id: &str| {
            let c = c.find_subcommand("s").filter(|_| id == "b").unwrap_or(c);
            let arg = c.get_arguments().find(|a| a.get_id() == id).unwrap();
            arg.get_env().unwrap().to_str().unwrap().to_owned()
        };

        let legacy_only = swap_env(cmd(), "NEW_", "OLD_", &|n| n.starts_with("OLD_"));
        assert_eq!(env_of(&legacy_only, "a"), "OLD_A");
        assert_eq!(env_of(&legacy_only, "b"), "OLD_B", "subcommands swap too");

        let both = swap_env(cmd(), "NEW_", "OLD_", &|_| true);
        assert_eq!(env_of(&both, "a"), "NEW_A");

        let same = swap_env(cmd(), "NEW_", "NEW_", &|_| false);
        assert_eq!(env_of(&same, "a"), "NEW_A");
    }
}
