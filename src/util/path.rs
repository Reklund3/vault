use std::path::{Path, PathBuf};

use crate::config::home_dir;

/// Expand a leading `~` (or `~/`) to the user's home directory. Anything else
/// returns unchanged. Pure — no filesystem access. A general path primitive
/// retained for the planned `toml_edit`-based first-run persistence (tasks
/// #3/#4), which expands `~` in user-supplied repo paths.
#[allow(dead_code)] // next consumer arrives with first-run vault.toml persistence
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
