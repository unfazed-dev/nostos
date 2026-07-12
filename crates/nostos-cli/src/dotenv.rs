//! A minimal `.env` reader/writer. Not a general dotenv implementation —
//! just enough to round-trip `KEY=VALUE` lines nostos-cli itself writes,
//! preserving any other lines a user added. Avoids pulling in a `dotenv`
//! crate for a handful of lines.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Parse a `.env` file into a key→value map. Missing file = empty map (not
/// an error — `.env` is optional until `init` writes one).
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
    vars
}

/// Set `key=value` in the `.env` at `path`, preserving every other line
/// verbatim (including comments and unrelated vars). Creates the file if
/// absent.
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
}
