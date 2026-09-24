//! A minimal `.env` reader/writer. Not a general dotenv implementation —
//! just enough to round-trip `KEY=VALUE` lines nostos-cli itself writes,
//! preserving any other lines a user added. Avoids pulling in a `dotenv`
//! crate for a handful of lines.
//!
//! Security posture (2026-08-17 security audit, plan task 4.1, finding 4):
//! these files carry p8 keys, FCM service-account JSON, and VAPID scalars,
//! so on unix a NEWLY created file is opened with mode 0o600 (owner-only).
//! A file that already exists with looser permissions is NEVER chmod-ed
//! silently — the writer prints a stderr warning naming the file and its
//! current mode and leaves fixing it to the operator.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Parse a `.env` file into a key→value map. Missing file = empty map (not
/// an error — `.env` is optional until `init` writes one). Pre-rename keys
/// also answer to their current names (ADR-0046).
#[must_use]
pub fn read(path: &Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return vars;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            vars.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    nostos_infra::env::fold_legacy(&mut vars);
    vars
}

/// Set `key=value` in the `.env` at `path`, preserving every other line
/// verbatim (including comments and unrelated vars). Creates the file if
/// absent — owner-only (0o600) on unix.
pub fn set(path: &Path, key: &str, value: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut found = false;
    let mut out_lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        if !found && !trimmed.starts_with('#') {
            if let Some((k, _)) = trimmed.split_once('=') {
                if k.trim() == key {
                    out_lines.push(format!("{key}={value}"));
                    found = true;
                    continue;
                }
            }
        }
        out_lines.push(line.to_string());
    }
    if !found {
        out_lines.push(format!("{key}={value}"));
    }
    let mut text = out_lines.join("\n");
    text.push('\n');
    write_private(path, &text)
}

/// The warning for an existing env file whose permissions are looser than
/// owner-only: Some(message) when group/others hold any permission bit.
/// Pure so the tests can assert the message without capturing stderr.
#[cfg(unix)]
fn loose_mode_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        Some(format!(
            "warning: {} is mode {:o} — group/others can read it and it holds push \\
             secrets; tighten it (e.g. chmod 600 {})",
            path.display(),
            mode,
            path.display()
        ))
    } else {
        None
    }
}

/// Write the env file: warn (do not fix) loose permissions on an existing
/// file; create new files owner-only. `.mode()` applies only at creation,
/// which is exactly the no-silent-chmod contract.
#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(warning) = loose_mode_warning(path) {
        eprintln!("{warning}");
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_empty() {
        let path = std::env::temp_dir().join("nostos-cli-dotenv-missing.env");
        std::fs::remove_file(&path).ok();
        assert!(read(&path).is_empty());
    }

    #[test]
    fn set_creates_then_updates_in_place() {
        let path =
            std::env::temp_dir().join(format!("nostos-cli-dotenv-{}.env", std::process::id()));
        std::fs::remove_file(&path).ok();

        set(&path, "NOSTOS_PG_URL", "postgresql://a").unwrap();
        set(&path, "OTHER", "keep-me").unwrap();
        set(&path, "NOSTOS_PG_URL", "postgresql://b").unwrap();

        let vars = read(&path);
        assert_eq!(vars.get("NOSTOS_PG_URL").unwrap(), "postgresql://b");
        assert_eq!(vars.get("OTHER").unwrap(), "keep-me");
        // Updating in place must not duplicate the line.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("NOSTOS_PG_URL").count(), 1);

        std::fs::remove_file(&path).ok();
    }

    // ---- audit finding 4 (plan task 4.1): secret files are owner-only ---

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn created_env_file_is_owner_only() {
        let path = std::env::temp_dir().join(format!(
            "nostos-cli-dotenv-mode-{}.env",
            uuid::Uuid::new_v4()
        ));
        std::fs::remove_file(&path).ok();
        set(&path, "NOSTOS_APNS_KEY_P8", "secret-material").unwrap();
        assert_eq!(
            file_mode(&path),
            0o600,
            "a NEW .env holding rail secrets must be owner-only"
        );
        // Rewriting the same (tight) file keeps it tight.
        set(&path, "NOSTOS_APNS_KEY_P8", "rotated").unwrap();
        assert_eq!(file_mode(&path), 0o600);
        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn loose_existing_env_warns_and_is_never_chmod_ed() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "nostos-cli-dotenv-loose-{}.env",
            uuid::Uuid::new_v4()
        ));
        std::fs::remove_file(&path).ok();
        std::fs::write(&path, "OLD=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // The warning fires on the loose file, naming it and its mode.
        let warning = loose_mode_warning(&path).expect("0o644 must warn");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(warning.contains(name.as_ref()), "names the file: {warning}");
        assert!(warning.contains("644"), "names the current mode: {warning}");

        // Writing through set() does NOT silently chmod: still 0o644.
        set(&path, "NOSTOS_FCM_CREDENTIALS_JSON", "{}").unwrap();
        assert_eq!(file_mode(&path), 0o644, "never chmod silently");

        // A tight (0o600) file does not warn.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(loose_mode_warning(&path).is_none(), "0o600 is quiet");
        std::fs::remove_file(&path).ok();
    }
}
