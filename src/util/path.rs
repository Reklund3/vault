use std::path::{Path, PathBuf};

use crate::config::home_dir;

/// Expand a leading `~` (or `~/`) to the user's home directory. Anything else
/// returns unchanged. Pure — no filesystem access. Used as the first step of
/// `normalize_repo_key` so vault.toml keys written as `~/repos/foo` line up
/// with what the sync command canonicalizes from the user's `$HOME`.
pub fn expand_tilde(input: &str) -> PathBuf {
    let Some(rest) = input.strip_prefix('~') else {
        return PathBuf::from(input);
    };
    let Some(home) = home_dir() else {
        return PathBuf::from(input);
    };
    if rest.is_empty() {
        return home;
    }
    // Accept both `~/foo` and `~foo` (the latter is unusual but cheap to handle).
    let trimmed = rest.strip_prefix('/').unwrap_or(rest);
    home.join(trimmed)
}

/// Canonicalize a repo path into the string form used as a `[classifications.*]`
/// key. Tilde-expands first (always), then attempts `std::fs::canonicalize`;
/// falls back to the expanded form on failure (the path may not exist on disk
/// — e.g. an archived repo entry in vault.toml — and we still want
/// deterministic, equal-on-both-sides keys).
///
/// Both `Config::cached_classification` (read) and the cache write-back at
/// 14.5 must run through this helper so the key generated on either side
/// agrees. Without it, a sync against `/Users/me/git/foo` would never match a
/// user-authored `~/git/foo` section, and write-back would create a duplicate
/// entry alongside the existing one.
pub fn normalize_repo_key(input: &str) -> String {
    let expanded = expand_tilde(input);
    let canonical = std::fs::canonicalize(&expanded).unwrap_or(expanded);
    canonical.to_string_lossy().to_string()
}

#[allow(dead_code)] // first consumer arrives with 14.6 walker integration
pub fn to_posix_relative(repo_root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(repo_root).ok()?;
    let mut out = String::with_capacity(rel.as_os_str().len());
    for (i, comp) in rel.components().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(comp.as_os_str().to_str()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn expand_tilde_returns_input_unchanged_when_no_tilde() {
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn expand_tilde_expands_leading_tilde() {
        let home = home_dir().expect("HOME must be set for tests");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/git/foo"), home.join("git/foo"));
    }

    #[test]
    fn normalize_repo_key_canonicalizes_real_path() {
        // Tempdir exists on disk — canonicalize resolves symlinks (e.g. macOS
        // `/var` → `/private/var`) so the returned key is the real path, not
        // whatever the caller passed.
        let dir = std::env::temp_dir();
        let key = normalize_repo_key(dir.to_str().expect("temp dir is utf8"));
        let expected = std::fs::canonicalize(&dir)
            .expect("temp dir canonicalizes")
            .to_string_lossy()
            .to_string();
        assert_eq!(key, expected);
    }

    #[test]
    fn normalize_repo_key_falls_back_when_path_missing() {
        // The path doesn't exist; we expect the tilde-expanded form back, not
        // a panic or an empty string. This is the path archived-repo entries
        // in vault.toml take.
        let key = normalize_repo_key("/definitely/not/a/real/path/zzz-vault-test");
        assert_eq!(key, "/definitely/not/a/real/path/zzz-vault-test");
    }

    #[test]
    fn normalize_repo_key_expands_tilde_in_fallback_path() {
        let home = home_dir().expect("HOME set");
        // A tilde-keyed entry pointing to a non-existent subpath should still
        // be expanded — that's how `~/git/archived/foo` keys keep working
        // even after the user deletes the directory.
        let key = normalize_repo_key("~/this-subdir-definitely-does-not-exist-zzz");
        let expected = home
            .join("this-subdir-definitely-does-not-exist-zzz")
            .to_string_lossy()
            .to_string();
        assert_eq!(key, expected);
    }

    #[test]
    fn to_posix_relative_uses_forward_slashes() {
        // Construct paths so the test works on any OS — strip_prefix is
        // OS-aware, the joiner output is always forward-slashed.
        let dir = std::env::temp_dir();
        let nested = dir.join("a").join("b").join("c.proto");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, b"").unwrap();
        let rel = to_posix_relative(&dir, &nested).expect("relative");
        assert_eq!(rel, "a/b/c.proto");
        let _ = fs::remove_file(&nested);
    }
}
