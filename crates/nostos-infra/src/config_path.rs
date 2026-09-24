//! On-disk config names that survive the project rename (ADR-0046).

use std::path::{Path, PathBuf};

/// `dir/name`, or `dir/legacy_name` when only the pre-rename file (or
/// directory) exists. Readers and writers both call this, so a legacy file
/// is edited in place and a second copy never appears; with neither present
/// the primary name is created. Picking the legacy path warns once.
#[must_use]
pub fn resolve(dir: &Path, name: &str, legacy_name: &str) -> PathBuf {
    let primary = dir.join(name);
    let legacy = dir.join(legacy_name);
    if !primary.exists() && legacy.exists() {
        crate::env::warn_once(
            &legacy.display().to_string(),
            &format!("rename it to {name}"),
        );
        legacy
    } else {
        primary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("config-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_only_resolves_to_legacy() {
        let dir = temp_dir();
        std::fs::write(dir.join("old.toml"), "").unwrap();
        assert_eq!(resolve(&dir, "new.toml", "old.toml"), dir.join("old.toml"));
    }

    #[test]
    fn primary_only_resolves_to_primary() {
        let dir = temp_dir();
        std::fs::write(dir.join("new.toml"), "").unwrap();
        assert_eq!(resolve(&dir, "new.toml", "old.toml"), dir.join("new.toml"));
    }

    #[test]
    fn both_resolve_to_primary() {
        let dir = temp_dir();
        std::fs::write(dir.join("new.toml"), "").unwrap();
        std::fs::write(dir.join("old.toml"), "").unwrap();
        assert_eq!(resolve(&dir, "new.toml", "old.toml"), dir.join("new.toml"));
    }

    #[test]
    fn neither_resolves_to_primary_so_writers_create_it() {
        let dir = temp_dir();
        assert_eq!(resolve(&dir, "new.toml", "old.toml"), dir.join("new.toml"));
    }

    #[test]
    fn directories_resolve_too() {
        let dir = temp_dir();
        std::fs::create_dir(dir.join(".old")).unwrap();
        assert_eq!(resolve(&dir, ".new", ".old"), dir.join(".old"));
    }
}
