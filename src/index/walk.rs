use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use thiserror::Error;
use walkdir::WalkDir;

/// Files larger than this are skipped by the walker. Embedding models don't
/// usefully ingest megabyte-scale binaries, and pulling them through TEI burns
/// time without producing useful retrieval. Override at the call site if a
/// later use case justifies it.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Non-removable exclusions enforced regardless of `[indexer.exclude]`. CLAUDE.md
/// treats this list as a security boundary, not a convenience — entries here
/// are the ones that *must not* leak into an index even if the user's vault.toml
/// is empty. User-provided patterns from `WalkOptions::user_extra_excludes` are
/// applied in addition to this list, never instead of.
const BUILT_IN_EXCLUDES: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
    "id_ecdsa*",
    "**/.ssh/**",
    "**/.aws/**",
    "**/.gnupg/**",
    "**/.git/**",
    "**/node_modules/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.cache/**",
    "**/.DS_Store",
];

#[derive(Debug, Default, Clone)]
pub struct WalkOptions {
    /// User-supplied glob additions from `[indexer.exclude].patterns`. Applied
    /// on top of `BUILT_IN_EXCLUDES`; never removes a built-in.
    pub user_extra_excludes: Vec<String>,
}

/// One file the walker chose to surface to the indexer.
#[derive(Debug, Clone)]
pub struct Walked {
    /// Absolute path with symlinks resolved. Even though we walk with
    /// `follow_links(false)`, individual components of the root might be
    /// symlinks (e.g. `/var` → `/private/var` on macOS); canonicalizing here
    /// gives the rest of the pipeline a stable identity.
    pub canonical_path: PathBuf,
    /// POSIX-style path relative to the canonical repo root. The classifier
    /// cache and the eventual `documents.source_path` both key off this form,
    /// so platform separators are normalized to `/` here.
    pub relative_path: String,
}

#[derive(Debug, Error)]
pub enum WalkError {
    #[error("repo path is not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error("could not canonicalize repo path {path:?}: {source}")]
    Canonicalize { path: PathBuf, source: std::io::Error },
    #[error("invalid exclusion glob {pattern:?}: {detail}")]
    BadGlob { pattern: String, detail: String },
}

/// Walk `root` and return every file the indexer should consider. Enforces the
/// non-removable security rules at this layer so callers can't accidentally
/// opt out:
///
/// - **No symlinks.** `WalkDir::new(...).follow_links(false)`. Plus a defense
///   in depth: any file whose canonical path escapes the canonical root is
///   silently dropped, so a malicious in-repo symlink chain still can't reach
///   `~/.ssh/id_rsa`.
/// - **Built-in excludes are non-removable.** `BUILT_IN_EXCLUDES` always
///   applies; user extras add to it.
/// - **Oversize skip** at `MAX_FILE_BYTES` — currently 1 MiB. Skipped silently
///   (the report-level counter is the indexer's job, not the walker's).
pub fn walk_repo(root: &Path, opts: &WalkOptions) -> Result<Vec<Walked>, WalkError> {
    let canonical_root = std::fs::canonicalize(root).map_err(|e| WalkError::Canonicalize {
        path: root.to_path_buf(),
        source: e,
    })?;
    if !canonical_root.is_dir() {
        return Err(WalkError::NotADirectory(canonical_root));
    }

    let excludes = build_exclude_set(&opts.user_extra_excludes)?;

    let mut out = Vec::new();
    for entry in WalkDir::new(&canonical_root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        // Defense in depth: even with follow_links(false), reject anything
        // whose canonical form lands outside the canonical root.
        let Ok(canonical_path) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if !canonical_path.starts_with(&canonical_root) {
            continue;
        }

        let Some(rel) = posix_relative(&canonical_root, &canonical_path) else {
            continue;
        };

        if excludes.is_match(&rel) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }

        out.push(Walked {
            canonical_path,
            relative_path: rel,
        });
    }

    Ok(out)
}

fn build_exclude_set(user_extras: &[String]) -> Result<GlobSet, WalkError> {
    let mut builder = GlobSetBuilder::new();
    for pat in BUILT_IN_EXCLUDES {
        let glob = Glob::new(pat).map_err(|e| WalkError::BadGlob {
            pattern: (*pat).to_string(),
            detail: e.to_string(),
        })?;
        builder.add(glob);
    }
    for pat in user_extras {
        let glob = Glob::new(pat).map_err(|e| WalkError::BadGlob {
            pattern: pat.clone(),
            detail: e.to_string(),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| WalkError::BadGlob {
        pattern: "<set>".to_string(),
        detail: e.to_string(),
    })
}

/// POSIX-style relative path of `file` against `root`. Returns `None` if
/// `file` is not actually inside `root` or contains a non-utf8 segment.
fn posix_relative(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
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

    /// One temp dir per test; cleaned up on drop. Uses a random subdir of
    /// `std::env::temp_dir()` so concurrent test runs don't collide.
    struct Tmp {
        root: PathBuf,
    }
    impl Tmp {
        fn new(label: &str) -> Self {
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!("vault-walk-{label}-{pid}-{nanos}"));
            fs::create_dir_all(&root).expect("create tempdir");
            Self { root }
        }
        fn write(&self, rel: &str, body: &[u8]) -> PathBuf {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, body).unwrap();
            path
        }
        fn mkdir(&self, rel: &str) -> PathBuf {
            let p = self.root.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn rels(walked: &[Walked]) -> Vec<String> {
        let mut v: Vec<String> = walked.iter().map(|w| w.relative_path.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn walks_plain_files_under_root() {
        let tmp = Tmp::new("plain");
        tmp.write("a.proto", b"x");
        tmp.write("nested/b.go", b"y");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(rels(&out), vec!["a.proto".to_string(), "nested/b.go".to_string()]);
    }

    #[test]
    fn excludes_dot_env_and_pem_files() {
        let tmp = Tmp::new("env");
        tmp.write(".env", b"SECRET=x");
        tmp.write(".env.local", b"SECRET=y");
        tmp.write("cert.pem", b"-----BEGIN");
        tmp.write("keep.go", b"package main");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(rels(&out), vec!["keep.go".to_string()]);
    }

    #[test]
    fn excludes_recursive_directories() {
        let tmp = Tmp::new("dirs");
        tmp.write("src/main.rs", b"fn main() {}");
        tmp.write("node_modules/foo/index.js", b"//");
        tmp.write("target/debug/build.txt", b"//");
        tmp.write(".git/config", b"[core]");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(rels(&out), vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn user_extras_add_to_built_ins() {
        let tmp = Tmp::new("extras");
        tmp.write("keep.go", b"//");
        tmp.write("ignored.log", b"//");
        let opts = WalkOptions {
            user_extra_excludes: vec!["*.log".to_string()],
        };
        let out = walk_repo(&tmp.root, &opts).unwrap();
        assert_eq!(rels(&out), vec!["keep.go".to_string()]);
        // Built-ins still apply even with user extras present.
        tmp.write(".env", b"x");
        let out = walk_repo(&tmp.root, &opts).unwrap();
        assert!(!rels(&out).contains(&".env".to_string()));
    }

    #[test]
    fn skips_files_over_max_bytes() {
        let tmp = Tmp::new("large");
        tmp.write("small.txt", &vec![b'x'; 100]);
        tmp.write("huge.bin", &vec![b'x'; (MAX_FILE_BYTES + 1) as usize]);
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(rels(&out), vec!["small.txt".to_string()]);
    }

    #[test]
    fn does_not_follow_symlinks_into_other_directories() {
        // Build two tempdirs: the indexed repo, and a sibling holding a
        // "secret". Place a symlink inside the repo pointing at the sibling's
        // file. The walker must not surface the secret.
        let repo = Tmp::new("symlink-repo");
        let sibling = Tmp::new("symlink-sibling");
        repo.write("README.md", b"hi");
        let secret = sibling.write("secret.txt", b"shh");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, repo.root.join("link.txt")).unwrap();

        // On non-Unix targets, the test reduces to the basic walk; we still
        // run it to keep the code path exercised.
        let out = walk_repo(&repo.root, &WalkOptions::default()).unwrap();
        let rels = rels(&out);
        assert!(!rels.contains(&"link.txt".to_string()), "symlink leaked: {rels:?}");
        assert!(rels.contains(&"README.md".to_string()));
    }

    #[test]
    fn rejects_non_directory_root() {
        let tmp = Tmp::new("not-dir");
        let file = tmp.write("only-file.txt", b"x");
        let err = walk_repo(&file, &WalkOptions::default()).unwrap_err();
        assert!(matches!(err, WalkError::NotADirectory(_)));
    }

    #[test]
    fn errors_on_missing_root() {
        let bogus = PathBuf::from("/definitely/not/here/zzz-vault");
        let err = walk_repo(&bogus, &WalkOptions::default()).unwrap_err();
        assert!(matches!(err, WalkError::Canonicalize { .. }));
    }

    #[test]
    fn empty_directory_yields_empty_walk() {
        let tmp = Tmp::new("empty");
        tmp.mkdir("subdir");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert!(out.is_empty());
    }
}
