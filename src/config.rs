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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    /// HTTP timeout for one classifier call. Sync time, not hot path, so a
    /// generous value is fine — large local models (e.g. Gemma 4 31b bf16) can
    /// need 30–90s per call once warm, and longer on cold-load.
    timeout_secs: u32,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            model: "haiku".to_string(),
            timeout_secs: 300,
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
    /// Command `vault tei start` runs to spawn the TEI server. Optional so
    /// configs that manage TEI by hand still load; when unset, `vault tei start`
    /// errors with guidance instead of spawning.
    #[serde(default)]
    launcher_cmd: Option<String>,
}

/// Optional `[indexer]` block. Today it carries only `[indexer.exclude]`; if/when
/// the indexer grows more knobs they land here.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct Indexer {
    #[serde(default)]
    exclude: IndexerExclude,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct IndexerExclude {
    #[serde(default)]
    patterns: Vec<String>,
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
    // Optional so configs without an [indexer] block still load — the walker
    // falls back to the built-in exclusion list.
    #[serde(default)]
    indexer: Indexer,
}

// Todo: Move to a helper file/dir
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from)
}

/// `~/.vault` resolved from the home directory alone — no loaded `Config`
/// required. The hook's logger needs a destination precisely when
/// `vault.toml` failed to load, so this must not depend on `Config`.
pub(crate) fn vault_dir_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(CONFIG_DIR))
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

    /// The `[embeddings].launcher_cmd` string, if set and non-empty. Consumed by
    /// `vault tei start`. A whitespace-only value collapses to `None` so the
    /// start path errors with the same guidance as an absent key.
    pub fn embedding_launcher_cmd(&self) -> Option<&str> {
        self.embeddings
            .launcher_cmd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn classifier_mode(&self) -> &str {
        &self.classifier.mode
    }

    pub fn classifier_model(&self) -> &str {
        &self.classifier.model
    }

    pub fn classifier_timeout(&self) -> Duration {
        Duration::from_secs(self.classifier.timeout_secs as u64)
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

    /// User-supplied extra exclusion globs from `[indexer.exclude].patterns`.
    /// These are added to the walker's non-removable `BUILT_IN_EXCLUDES`; an
    /// empty vec means "use the built-ins only".
    pub fn indexer_exclude_patterns(&self) -> &[String] {
        &self.indexer.exclude.patterns
    }

    /// The global fallback context tag (`defaults.context_tag`), used for the
    /// injected block when no router-named project has a domain assignment in
    /// vault.db. Per-domain tags are derived by convention as `{domain}-context`
    /// in the hook, not configured here.
    pub fn default_context_tag(&self) -> &str {
        &self.defaults.context_tag
    }

    /// `~/.vault/` — the directory holding `vault.db`, `vault.toml`, and the TEI
    /// `tei.pid` / `tei.log` files. Does not create the directory; callers that
    /// write into it (e.g. the TEI launcher) are responsible for `create_dir_all`
    /// and permission hardening.
    pub fn vault_dir(&self) -> Result<PathBuf, ConfigError> {
        vault_dir_path().ok_or(ConfigError::HomeNotFound)
    }

    pub fn db_path(&self) -> Result<PathBuf, ConfigError> {
        Ok(self.vault_dir()?.join(CONFIG_DB))
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
                launcher_cmd: None,
            },
            indexer: Indexer::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_context_tag_returns_defaults_value() {
        // Domain-driven tags (`{domain}-context`) are resolved in the hook from
        // vault.db; config only supplies the unassigned fallback.
        let cfg = Config::default();
        assert_eq!(cfg.default_context_tag(), "vault-context");
    }

    // ----- indexer_exclude_patterns -----

    #[test]
    fn indexer_exclude_patterns_default_is_empty() {
        let cfg = Config::default();
        assert!(cfg.indexer_exclude_patterns().is_empty());
    }

    #[test]
    fn indexer_exclude_patterns_parses_from_toml() {
        let toml_text = r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15
timeout = 3

[router]
mode = "auto"
model = "haiku"

[mlx]
endpoint = "http://localhost:8080"
router_model = "test"

[embeddings]
endpoint = "http://localhost:8081"
model = "nomic-ai/nomic-embed-text-v1.5"
dims = 768

[indexer.exclude]
patterns = ["*.log", "tmp/**"]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(
            cfg.indexer_exclude_patterns(),
            &["*.log".to_string(), "tmp/**".to_string()]
        );
    }

    #[test]
    fn indexer_section_optional_for_back_compat() {
        // Existing vault.toml files with no [indexer] block must still load.
        let toml_text = r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15
timeout = 3

[router]
mode = "auto"
model = "haiku"

[mlx]
endpoint = "http://localhost:8080"
router_model = "test"

[embeddings]
endpoint = "http://localhost:8081"
model = "nomic-ai/nomic-embed-text-v1.5"
dims = 768
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert!(cfg.indexer_exclude_patterns().is_empty());
    }
}
