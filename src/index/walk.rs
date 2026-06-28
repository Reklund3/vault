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
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
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

    let gitignore = gitignore_globs_for_root(&canonical_root);
    let excludes = build_exclude_set(&opts.user_extra_excludes, &gitignore)?;

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

fn build_exclude_set(user_extras: &[String], gitignore: &[String]) -> Result<GlobSet, WalkError> {
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
    // Globs derived from the repo's root `.gitignore` are best-effort, not a
    // security boundary: a pattern we can't translate is skipped rather than
    // failing the whole sync. The built-in list above is what actually protects
    // secrets, and it always applies regardless of `.gitignore` contents.
    for pat in gitignore {
        match Glob::new(pat) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(_) => continue,
        }
    }
    builder.build().map_err(|e| WalkError::BadGlob {
        pattern: "<set>".to_string(),
        detail: e.to_string(),
    })
}

/// Read the repo's root `.gitignore` (if present) and translate it into globset
/// patterns matched against the POSIX relative path. This is a deliberately
/// scoped subset of git's ignore semantics: **root `.gitignore` only** — no
/// nested per-directory `.gitignore`, no global `core.excludesfile`, no
/// `.git/info/exclude`. Negation lines (`!pattern`) are **not** supported
/// (globset has no ordered-override notion) and are skipped, so a `.gitignore`
/// that re-includes a path via `!` will still see it excluded here. A missing or
/// unreadable file yields no patterns (the walk proceeds on built-ins alone).
fn gitignore_globs_for_root(root: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) else {
        return Vec::new();
    };
    let mut globs = Vec::new();
    for line in content.lines() {
        globs.extend(gitignore_line_to_globs(line));
    }
    globs
}

/// Translate a single `.gitignore` line into zero or more globset patterns.
/// Returns empty for blanks, comments, and negations.
///
/// A pattern that is leading-slash-anchored or contains an interior `/` is
/// anchored to the repo root; a bare name (no `/`) matches at any depth, so it
/// expands to both the anchored and `**/`-prefixed forms. Each pattern also
/// emits a `/**` subtree form so an ignored directory excludes its contents
/// (the walker only surfaces files, so the bare form alone wouldn't).
fn gitignore_line_to_globs(raw: &str) -> Vec<String> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return Vec::new();
    }
    let had_leading_slash = line.starts_with('/');
    let core = line.trim_start_matches('/').trim_end_matches('/');
    if core.is_empty() {
        return Vec::new();
    }
    let anchored = had_leading_slash || core.contains('/');
    if anchored {
        vec![core.to_string(), format!("{core}/**")]
    } else {
        vec![
            core.to_string(),
            format!("{core}/**"),
            format!("**/{core}"),
            format!("**/{core}/**"),
        ]
    }
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
        assert_eq!(
            rels(&out),
            vec!["a.proto".to_string(), "nested/b.go".to_string()]
        );
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
    fn respects_root_gitignore_directory_and_anchored_patterns() {
        let tmp = Tmp::new("gitignore");
        // The exact shape that started this: an ignored IDE dir leaking in.
        tmp.write(
            ".gitignore",
            b"# JetBrains\n.idea\n\n/target\n.claude/settings.local.json\n",
        );
        tmp.write(".idea/workspace.xml", b"<x/>");
        tmp.write(".idea/misc.xml", b"<x/>");
        tmp.write("target/debug/out.txt", b"x");
        tmp.write(".claude/settings.local.json", b"{}");
        tmp.write(".claude/skills/keep.md", b"# kept"); // not ignored — must survive
        tmp.write("src/main.rs", b"fn main() {}");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        // `.gitignore` itself is not matched by any pattern, so it survives.
        assert_eq!(
            rels(&out),
            vec![
                ".claude/skills/keep.md".to_string(),
                ".gitignore".to_string(),
                "src/main.rs".to_string()
            ]
        );
    }

    #[test]
    fn gitignore_ignores_comments_blanks_and_negations() {
        let tmp = Tmp::new("gitignore-edge");
        // Negation is unsupported: `keep.log` stays excluded by `*.log`, not
        // re-included. Comment and blank lines contribute nothing.
        tmp.write(".gitignore", b"# a comment\n\n*.log\n!keep.log\n");
        tmp.write("app.log", b"x");
        tmp.write("keep.log", b"x");
        tmp.write("main.rs", b"fn main() {}");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(
            rels(&out),
            vec![".gitignore".to_string(), "main.rs".to_string()]
        );
    }

    #[test]
    fn malformed_gitignore_line_is_skipped_not_fatal() {
        let tmp = Tmp::new("gitignore-bad");
        // An unclosed character class is an invalid glob; it must be dropped
        // silently rather than aborting the walk.
        tmp.write(".gitignore", b"[\nbadname\n");
        tmp.write("badname", b"x");
        tmp.write("keep.rs", b"fn main() {}");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(
            rels(&out),
            vec![".gitignore".to_string(), "keep.rs".to_string()]
        );
    }

    #[test]
    fn no_gitignore_walks_normally() {
        let tmp = Tmp::new("gitignore-none");
        tmp.write("a.rs", b"//");
        tmp.write("b.go", b"//");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(rels(&out), vec!["a.rs".to_string(), "b.go".to_string()]);
    }

    #[test]
    fn built_in_excludes_apply_even_if_gitignore_omits_them() {
        let tmp = Tmp::new("gitignore-floor");
        // `.gitignore` says nothing about secrets; the built-in floor still drops them.
        tmp.write(".gitignore", b"*.log\n");
        tmp.write(".env", b"SECRET=x");
        tmp.write("cert.pem", b"-----BEGIN");
        tmp.write("keep.rs", b"//");
        let out = walk_repo(&tmp.root, &WalkOptions::default()).unwrap();
        assert_eq!(
            rels(&out),
            vec![".gitignore".to_string(), "keep.rs".to_string()]
        );
    }

    #[test]
    fn skips_files_over_max_bytes() {
        let tmp = Tmp::new("large");
        tmp.write("small.txt", &[b'x'; 100]);
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
        assert!(
            !rels.contains(&"link.txt".to_string()),
            "symlink leaked: {rels:?}"
        );
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
