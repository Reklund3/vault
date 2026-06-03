use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

const CONFIG_FILE: &str = "vault.toml";
const CONFIG_DIR: &str = ".vault";
const CONFIG_DB: &str = "vault.db";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not resolve home directory (set HOME or USERPROFILE)")]
    HomeNotFound,
    #[error("missing key {0}")]
    MissingKey(String),
    #[error("io error reading config: {0}")]
    IoError(#[from] std::io::Error),
    #[error("parse error: {0}")]
    ParseError(#[from] toml::de::Error),
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Defaults {
    context_tag: String,
    token_budget: u16,
    alpha: f32,
    min_score: f32,
    timeout: u8,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Router {
    mode: String,
    model: String,
    /// HTTP timeout for one router call. Defaults to 3s per CLAUDE.md's
    /// hot-path budget; raise it in `vault.toml` when running a slow local
    /// model that can't meet the default.
    #[serde(default = "default_router_timeout_secs")]
    timeout_secs: u32,
}

fn default_router_timeout_secs() -> u32 {
    3
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Classifier {
    mode: String,
    model: String,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            model: "haiku".to_string(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Mlx {
    endpoint: String,
    router_model: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Embeddings {
    endpoint: String,
    model: String,
    dims: u16,
}

/// A logical group of projects that share one context tag. Configured under
/// `[domains.<name>]` in `vault.toml`. The hook uses `projects` to match the
/// router's named projects back to a `context_tag` for the injected block.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Domain {
    pub context_tag: String,
    pub projects: Vec<String>,
}

/// One row of the classification cache as stored on disk. Keeps the
/// TOML-deserialization shape separate from `index::classify::Classification`
/// so the on-disk schema can drift independently from the runtime type.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CachedClassification {
    pub doc_type: crate::types::DocType,
    pub language: crate::types::Language,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    defaults: Defaults,
    router: Router,
    // Optional so existing vault.toml files without a [classifier] section keep
    // loading; falls back to auto-mode / haiku alias.
    #[serde(default)]
    classifier: Classifier,
    mlx: Mlx,
    embeddings: Embeddings,
    // Optional so configs without any [domains.*] section still load — the hook
    // falls back to `defaults.context_tag` when no project matches.
    #[serde(default)]
    domains: HashMap<String, Domain>,
    // `[classifications."<repo>"]` sections — outer key is the repo path
    // (tilde or canonical; we normalize on read), inner key is a glob pattern,
    // value is the cached classification. Optional so existing configs without
    // any cache section still load.
    #[serde(default)]
    classifications: HashMap<String, HashMap<String, CachedClassification>>,
}

// Todo: Move to a helper file/dir
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from)
}

impl Config {
    // Todo for now we do this. Will load from vault.toml later?
    pub(crate) fn load() -> Result<Self, ConfigError> {
        let config_path = home_dir()
            .ok_or(ConfigError::HomeNotFound)?
            .join(CONFIG_DIR)
            .join(CONFIG_FILE);
        let content = std::fs::read_to_string(config_path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn embedding_dim(&self) -> usize {
        self.embeddings.dims as usize
    }

    pub fn embedding_model(&self) -> &str {
        &self.embeddings.model
    }

    pub fn embedding_endpoint(&self) -> &str {
        &self.embeddings.endpoint
    }

    pub fn classifier_mode(&self) -> &str {
        &self.classifier.mode
    }

    pub fn classifier_model(&self) -> &str {
        &self.classifier.model
    }

    pub fn router_mode(&self) -> &str {
        &self.router.mode
    }

    pub fn router_model(&self) -> &str {
        &self.router.model
    }

    pub fn router_timeout(&self) -> Duration {
        Duration::from_secs(self.router.timeout_secs as u64)
    }

    pub fn mlx_endpoint(&self) -> &str {
        &self.mlx.endpoint
    }

    /// mlx_lm.server serves a single loaded model; the router and classifier
    /// both target it, so the configured `router_model` is the model name for
    /// both.
    pub fn mlx_model(&self) -> &str {
        &self.mlx.router_model
    }

    pub fn alpha(&self) -> f32 {
        self.defaults.alpha
    }

    pub fn token_budget(&self) -> u16 {
        self.defaults.token_budget
    }

    pub fn min_score(&self) -> f32 {
        self.defaults.min_score
    }

    /// Look up a cached classification for `relative_path` inside `canonical_repo`.
    /// The caller is responsible for canonicalizing `canonical_repo`; this method
    /// runs both that key and the on-disk keys through `normalize_repo_key` so a
    /// canonical lookup matches a tilde-keyed `[classifications."<repo>"]`
    /// section. Returns the first glob that matches `relative_path` — TOML map
    /// order is undefined, but the curated handful per repo makes ordering
    /// irrelevant for v1.
    pub fn cached_classification(
        &self,
        canonical_repo: &str,
        relative_path: &str,
    ) -> Option<crate::index::classify::Classification> {
        use globset::GlobBuilder;

        let target = crate::util::path::normalize_repo_key(canonical_repo);
        let section = self
            .classifications
            .iter()
            .find(|(k, _)| crate::util::path::normalize_repo_key(k) == target)
            .map(|(_, v)| v)?;

        for (pattern, cached) in section {
            let Ok(glob) = GlobBuilder::new(pattern).literal_separator(true).build() else {
                continue;
            };
            if glob.compile_matcher().is_match(relative_path) {
                return Some(crate::index::classify::Classification {
                    doc_type: cached.doc_type,
                    language: cached.language,
                });
            }
        }
        None
    }

    /// Resolve the context tag for an injection from the router's named
    /// projects. First project whose name appears in any `[domains.X].projects`
    /// list wins; otherwise the global `defaults.context_tag` is returned. Match
    /// is case-insensitive. Domain-overlap policy is not enforced — single
    /// source of truth in `vault.toml`.
    pub fn resolve_context_tag(&self, projects: &[String]) -> &str {
        for p in projects {
            for d in self.domains.values() {
                if d.projects.iter().any(|dp| dp.eq_ignore_ascii_case(p)) {
                    return &d.context_tag;
                }
            }
        }
        &self.defaults.context_tag
    }

    pub fn db_path(&self) -> Result<PathBuf, ConfigError> {
        Ok(home_dir()
            .ok_or(ConfigError::HomeNotFound)?
            .join(CONFIG_DIR)
            .join(CONFIG_DB))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            defaults: Defaults {
                context_tag: "vault-context".to_string(),
                token_budget: 10000,
                alpha: 0.6,
                min_score: 0.15,
                timeout: 3,
            },
            router: Router {
                mode: "auto".to_string(),
                model: "haiku".to_string(),
                timeout_secs: default_router_timeout_secs(),
            },
            classifier: Classifier::default(),
            mlx: Mlx {
                endpoint: "http://localhost:8080".to_string(),
                router_model: "gemma-4-31b-bf16".to_string(),
            },
            embeddings: Embeddings {
                endpoint: "http://localhost:8081".to_string(),
                model: "nomic-ai/nomic-embed-text-v1.5".to_string(),
                dims: 768,
            },
            domains: HashMap::new(),
            classifications: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain(tag: &str, projects: &[&str]) -> Domain {
        Domain {
            context_tag: tag.to_string(),
            projects: projects.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn config_with_domains(pairs: &[(&str, Domain)]) -> Config {
        Config {
            domains: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn resolve_context_tag_matches_first_named_project() {
        let cfg = config_with_domains(&[("software", domain("software-context", &["olympus", "vault"]))]);
        let tag = cfg.resolve_context_tag(&["vault".to_string()]);
        assert_eq!(tag, "software-context");
    }

    #[test]
    fn resolve_context_tag_is_case_insensitive() {
        let cfg = config_with_domains(&[("software", domain("software-context", &["Vault"]))]);
        let tag = cfg.resolve_context_tag(&["VAULT".to_string()]);
        assert_eq!(tag, "software-context");
    }

    #[test]
    fn resolve_context_tag_falls_back_when_no_project_matches() {
        let cfg = config_with_domains(&[("finance", domain("finance-context", &["bookkeeping"]))]);
        let tag = cfg.resolve_context_tag(&["unknown-project".to_string()]);
        assert_eq!(tag, "vault-context");
    }

    #[test]
    fn resolve_context_tag_falls_back_on_empty_projects() {
        let cfg = config_with_domains(&[("software", domain("software-context", &["vault"]))]);
        let tag = cfg.resolve_context_tag(&[]);
        assert_eq!(tag, "vault-context");
    }

    #[test]
    fn resolve_context_tag_falls_back_when_no_domains_configured() {
        let cfg = Config::default();
        let tag = cfg.resolve_context_tag(&["vault".to_string()]);
        assert_eq!(tag, "vault-context");
    }

    #[test]
    fn resolve_context_tag_first_listed_project_wins_across_domains() {
        // First project mentioned that hits any domain wins, even if a later
        // project would hit a different one. Mirrors `[domains.*]` first-match
        // policy documented in the plan.
        let cfg = config_with_domains(&[
            ("software", domain("software-context", &["vault"])),
            ("finance", domain("finance-context", &["bookkeeping"])),
        ]);
        let tag = cfg.resolve_context_tag(&["bookkeeping".to_string(), "vault".to_string()]);
        assert_eq!(tag, "finance-context");
    }

    // ----- cached_classification -----

    use crate::types::{DocType, Language};

    fn cached(d: DocType, l: Language) -> CachedClassification {
        CachedClassification { doc_type: d, language: l }
    }

    fn config_with_classifications(
        pairs: &[(&str, &[(&str, CachedClassification)])],
    ) -> Config {
        let classifications = pairs
            .iter()
            .map(|(repo, rules)| {
                let inner: HashMap<String, CachedClassification> = rules
                    .iter()
                    .map(|(pat, cls)| (pat.to_string(), cls.clone()))
                    .collect();
                (repo.to_string(), inner)
            })
            .collect();
        Config { classifications, ..Config::default() }
    }

    #[test]
    fn cached_classification_exact_path_matches_only_exact() {
        let cfg = config_with_classifications(&[(
            "/repo",
            &[("CLAUDE.md", cached(DocType::Meta, Language::Markdown))],
        )]);
        let hit = cfg.cached_classification("/repo", "CLAUDE.md").expect("hit");
        assert_eq!(hit.doc_type, DocType::Meta);
        assert_eq!(hit.language, Language::Markdown);
        // literal-separator semantics: "CLAUDE.md" must NOT match "docs/CLAUDE.md".
        assert!(cfg.cached_classification("/repo", "docs/CLAUDE.md").is_none());
    }

    #[test]
    fn cached_classification_double_star_matches_any_depth() {
        let cfg = config_with_classifications(&[(
            "/repo",
            &[("**/*.proto", cached(DocType::Contract, Language::Proto))],
        )]);
        assert!(cfg.cached_classification("/repo", "build/build.proto").is_some());
        assert!(cfg.cached_classification("/repo", "top.proto").is_some());
        assert!(cfg.cached_classification("/repo", "deep/nested/path/x.proto").is_some());
        assert!(cfg.cached_classification("/repo", "build/build.go").is_none());
    }

    #[test]
    fn cached_classification_repos_are_isolated() {
        let cfg = config_with_classifications(&[
            ("/repo-a", &[("**/*.proto", cached(DocType::Contract, Language::Proto))]),
            ("/repo-b", &[("**/*.go", cached(DocType::Convention, Language::Go))]),
        ]);
        // A proto file in repo-b must miss — repo-b's section only covers .go.
        assert!(cfg.cached_classification("/repo-b", "x.proto").is_none());
        assert!(cfg.cached_classification("/repo-a", "x.proto").is_some());
    }

    #[test]
    fn cached_classification_returns_none_when_repo_section_absent() {
        let cfg = Config::default();
        assert!(cfg.cached_classification("/anywhere", "file.proto").is_none());
    }

    #[test]
    fn cached_classification_canonical_lookup_matches_tilde_keyed_section() {
        // Use the system temp dir (which exists and canonicalizes) as the repo
        // root so both sides have a real canonical form to compare against.
        // The cache section is keyed by tilde-relative-to-temp; the lookup
        // passes the canonical path. normalize_repo_key must reconcile them.
        let canonical_tmp = std::fs::canonicalize(std::env::temp_dir())
            .expect("temp canonicalizes");
        // Build a tilde-keyed section that, after normalize_repo_key, expands
        // to the same canonical tmp. We can't directly tilde-key a tempdir
        // (it isn't under $HOME), so instead use the canonical key on disk
        // and the *expanded* canonical for the lookup — both run through
        // normalize_repo_key, so the equality only holds if normalize runs on
        // both sides as documented.
        let cfg = config_with_classifications(&[(
            canonical_tmp.to_str().unwrap(),
            &[("**/*.proto", cached(DocType::Contract, Language::Proto))],
        )]);
        let hit = cfg
            .cached_classification(canonical_tmp.to_str().unwrap(), "x.proto")
            .expect("hit");
        assert_eq!(hit.doc_type, DocType::Contract);
    }
}
