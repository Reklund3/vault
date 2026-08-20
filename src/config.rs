use std::path::{Path, PathBuf};
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
    #[error("io error reading config {}: {source}", path.display())]
    IoError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse error in {}: {source}", path.display())]
    ParseError {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Defaults {
    context_tag: String,
    token_budget: u16,
    alpha: f32,
    min_score: f32,
    /// Hard cap on how many chunks the budget pass selects, highest
    /// `final_score` first. `#[serde(default)]` so existing `vault.toml` files
    /// keep loading, and `None` means uncapped — the historical behaviour,
    /// where only `token_budget` and `min_score` bound the selection.
    #[serde(default)]
    max_hits: Option<u16>,
}

// `#[serde(default)]` on the struct lets a present `[router]`/`[classifier]`
// block omit any field — the missing ones come from the `Default` impl below,
// so no per-field default functions are needed.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
struct Router {
    mode: String,
    model: String,
    /// HTTP timeout in seconds for one router call. Hot path — defaults to 3;
    /// raise it in `vault.toml` for a slow local model that can't meet that.
    timeout: u32,
    /// Which remote backend `auto` falls back to when the local mlx server is
    /// unreachable: `"haiku"` (default, back-compat) or `"openai"` (the generic
    /// OpenAI-compatible backend below — e.g. Gemini via a static key).
    remote: String,
    /// OpenAI-compatible chat-completions base URL for the `openai` backend.
    /// Defaults to the AI Studio Gemini endpoint. Set to
    /// `https://aiplatform.googleapis.com/v1` for Vertex express.
    base_url: String,
    /// Name of the environment variable holding the API key for the `openai`
    /// backend. The key itself is NEVER stored here — only the var name. Read at
    /// construction via `std::env::var`.
    api_key_env: String,
    /// Auth header style for the `openai` backend: `"bearer"` (AI Studio Gemini)
    /// or `"x-goog-api-key"` (Vertex express).
    auth_header: String,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            model: "haiku".to_string(),
            timeout: 3,
            remote: "haiku".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            api_key_env: "GEMINI_API_KEY".to_string(),
            auth_header: "bearer".to_string(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
struct Classifier {
    mode: String,
    model: String,
    /// HTTP timeout in seconds for one classifier call. Sync time, not hot path,
    /// so generous is fine — large local models (e.g. Gemma 4 31b bf16) can need
    /// 30–90s per call once warm. Defaults to 300.
    timeout: u32,
    /// Remote fallback for `auto`: `"haiku"` (default) or `"openai"`. Mirrors
    /// `[router].remote`.
    remote: String,
    /// OpenAI-compatible base URL for the `openai` backend. See `[router].base_url`.
    base_url: String,
    /// Name of the env var holding the `openai` backend's API key (var name only,
    /// never the key). See `[router].api_key_env`.
    api_key_env: String,
    /// Auth header style for the `openai` backend: `"bearer"` or `"x-goog-api-key"`.
    auth_header: String,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            model: "haiku".to_string(),
            timeout: 300,
            remote: "haiku".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            api_key_env: "GEMINI_API_KEY".to_string(),
            auth_header: "bearer".to_string(),
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

    /// Where `vault.db`, `hook.log` and this file live.
    ///
    /// Not read from the toml — `#[serde(skip)]` — because it records where the
    /// config *came from*, which a file cannot know about itself. `None` means
    /// "resolve `~/.vault` from the environment at call time", which is the
    /// historical behaviour and what `Config::default()` gets.
    #[serde(skip)]
    vault_dir: Option<PathBuf>,
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
    /// Load `~/.vault/vault.toml`. The convenience constructor, and what every
    /// CLI subcommand uses.
    pub fn load() -> Result<Self, ConfigError> {
        let dir = vault_dir_path().ok_or(ConfigError::HomeNotFound)?;
        Self::from_path(dir)
    }

    /// Load `<dir>/vault.toml` and remember `dir` as this config's vault
    /// directory, so `vault_dir` and `db_path` resolve against it rather than
    /// against `$HOME`.
    ///
    /// This is the constructor for a consumer that is not the user's login
    /// shell — a service in a container, or one running as a different user,
    /// where `$HOME` either does not exist or is not where vault's data lives.
    pub fn from_path(dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let dir = dir.as_ref();
        let config_path = dir.join(CONFIG_FILE);
        let content =
            std::fs::read_to_string(&config_path).map_err(|source| ConfigError::IoError {
                path: config_path.clone(),
                source,
            })?;
        let mut config: Config =
            toml::from_str(&content).map_err(|source| ConfigError::ParseError {
                path: config_path,
                source,
            })?;
        config.vault_dir = Some(dir.to_path_buf());
        Ok(config)
    }

    /// Point an already-built `Config` at an explicit vault directory.
    ///
    /// The pair to `from_path` for a consumer that has no `vault.toml` at all —
    /// a container configured entirely from defaults still needs to say where
    /// `vault.db` goes.
    pub fn with_vault_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.vault_dir = Some(dir.as_ref().to_path_buf());
        self
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
        Duration::from_secs(self.classifier.timeout as u64)
    }

    pub fn classifier_remote(&self) -> &str {
        &self.classifier.remote
    }

    pub fn classifier_base_url(&self) -> &str {
        &self.classifier.base_url
    }

    pub fn classifier_api_key_env(&self) -> &str {
        &self.classifier.api_key_env
    }

    pub fn classifier_auth_header(&self) -> &str {
        &self.classifier.auth_header
    }

    pub fn router_mode(&self) -> &str {
        &self.router.mode
    }

    pub fn router_model(&self) -> &str {
        &self.router.model
    }

    pub fn router_timeout(&self) -> Duration {
        Duration::from_secs(self.router.timeout as u64)
    }

    pub fn router_remote(&self) -> &str {
        &self.router.remote
    }

    pub fn router_base_url(&self) -> &str {
        &self.router.base_url
    }

    pub fn router_api_key_env(&self) -> &str {
        &self.router.api_key_env
    }

    pub fn router_auth_header(&self) -> &str {
        &self.router.auth_header
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

    /// Cap on selected chunks, or `None` for uncapped. The third limit on the
    /// budget pass, alongside `token_budget` and `min_score`.
    pub fn max_hits(&self) -> Option<usize> {
        self.defaults.max_hits.map(usize::from)
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
        match &self.vault_dir {
            Some(dir) => Ok(dir.clone()),
            None => vault_dir_path().ok_or(ConfigError::HomeNotFound),
        }
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
                max_hits: None,
            },
            router: Router::default(),
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
            // No file was read, so there is nowhere to remember. `vault_dir()`
            // falls back to `$HOME` — call `with_vault_dir` to pin it.
            vault_dir: None,
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

    #[test]
    fn router_classifier_blocks_omitting_timeout_use_struct_defaults() {
        // A present [router]/[classifier] block may omit `timeout` — the struct's
        // `#[serde(default)]` fills it from the Default impl (3 / 300) instead of
        // hard-erroring (the live vault.toml foot-gun this closes).
        let toml_text = r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15

[router]
mode = "auto"
model = "haiku"

[classifier]
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
        assert_eq!(cfg.router_timeout(), Duration::from_secs(3));
        assert_eq!(cfg.classifier_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn openai_backend_fields_default_when_omitted() {
        // Existing vault.toml files predate the openai backend; omitting the new
        // fields must yield the back-compat defaults (remote=haiku) so behavior
        // is unchanged.
        let toml_text = r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15

[router]
mode = "auto"
model = "haiku"

[classifier]
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
        assert_eq!(cfg.router_remote(), "haiku");
        assert_eq!(cfg.router_auth_header(), "bearer");
        assert_eq!(cfg.router_api_key_env(), "GEMINI_API_KEY");
        assert_eq!(
            cfg.router_base_url(),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(cfg.classifier_remote(), "haiku");
    }

    #[test]
    fn openai_backend_fields_parse_from_toml() {
        let toml_text = r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15

[router]
mode = "auto"
model = "gemini-3.5-flash"
remote = "openai"
base_url = "https://aiplatform.googleapis.com/v1"
api_key_env = "VERTEX_API_KEY"
auth_header = "x-goog-api-key"

[classifier]
mode = "auto"
model = "gemini-3.5-flash"
remote = "openai"

[mlx]
endpoint = "http://localhost:8080"
router_model = "test"

[embeddings]
endpoint = "http://localhost:8081"
model = "nomic-ai/nomic-embed-text-v1.5"
dims = 768
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.router_remote(), "openai");
        assert_eq!(
            cfg.router_base_url(),
            "https://aiplatform.googleapis.com/v1"
        );
        assert_eq!(cfg.router_api_key_env(), "VERTEX_API_KEY");
        assert_eq!(cfg.router_auth_header(), "x-goog-api-key");
        assert_eq!(cfg.classifier_remote(), "openai");
    }

    #[test]
    fn explicit_timeout_overrides_the_default() {
        let toml_text = r#"
[defaults]
context_tag = "vault-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15

[router]
mode = "auto"
model = "haiku"
timeout = 5

[classifier]
mode = "auto"
model = "haiku"
timeout = 120

[mlx]
endpoint = "http://localhost:8080"
router_model = "test"

[embeddings]
endpoint = "http://localhost:8081"
model = "nomic-ai/nomic-embed-text-v1.5"
dims = 768
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.router_timeout(), Duration::from_secs(5));
        assert_eq!(cfg.classifier_timeout(), Duration::from_secs(120));
    }

    // ----- vault directory resolution (lib-split Step 7) -----

    const MINIMAL_TOML: &str = r#"
[defaults]
context_tag = "container-context"
token_budget = 10000
alpha = 0.6
min_score = 0.15

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

    /// One temp dir per test; cleaned up on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!(
                "vault-config-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("mkdir");
            Self(root)
        }
        fn write_config(&self, body: &str) -> &Path {
            std::fs::write(self.0.join(CONFIG_FILE), body).expect("write");
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn from_path_reads_the_toml_in_that_directory() {
        let tmp = TmpDir::new("reads");
        let dir = tmp.write_config(MINIMAL_TOML);

        let cfg = Config::from_path(dir).expect("load from explicit dir");
        assert_eq!(cfg.default_context_tag(), "container-context");
    }

    /// The whole point of Step 7: a consumer that named a directory gets that
    /// directory back, not `$HOME`. Asserting the exact paths is what makes this
    /// fail if `vault_dir` ever silently re-derives from the environment — and it
    /// does so without mutating `HOME`, which would race the other tests.
    #[test]
    fn from_path_pins_vault_dir_and_db_path_away_from_home() {
        let tmp = TmpDir::new("pins");
        let dir = tmp.write_config(MINIMAL_TOML);

        let cfg = Config::from_path(dir).expect("load");

        assert_eq!(cfg.vault_dir().expect("dir"), dir);
        assert_eq!(cfg.db_path().expect("db"), dir.join(CONFIG_DB));

        if let Some(home) = home_dir() {
            assert!(
                !cfg.db_path().expect("db").starts_with(home),
                "explicit dir must not resolve under $HOME"
            );
        }
    }

    /// A container configured entirely from defaults still has to say where
    /// `vault.db` goes, and it has no toml for `from_path` to read.
    #[test]
    fn with_vault_dir_pins_a_config_that_has_no_toml() {
        let dir = Path::new("/srv/vault-data");
        let cfg = Config::default().with_vault_dir(dir);

        assert_eq!(cfg.vault_dir().expect("dir"), dir);
        assert_eq!(cfg.db_path().expect("db"), dir.join(CONFIG_DB));
    }

    /// `load()` keeps the historical behaviour, and an unpinned `Config` still
    /// resolves `~/.vault` — the hook's logger and `vault configure` depend on
    /// that path existing without a config to consult.
    #[test]
    fn an_unpinned_config_still_resolves_under_home() {
        let cfg = Config::default();
        match home_dir() {
            Some(home) => assert_eq!(cfg.vault_dir().expect("dir"), home.join(CONFIG_DIR)),
            None => assert!(matches!(cfg.vault_dir(), Err(ConfigError::HomeNotFound))),
        }
    }

    /// A consumer juggling two config directories needs to know *which* one
    /// failed. An unqualified "No such file or directory" does not say.
    #[test]
    fn a_missing_config_names_the_path_it_looked_at() {
        let tmp = TmpDir::new("missing");
        let err = Config::from_path(&tmp.0).expect_err("no toml written");

        let rendered = err.to_string();
        assert!(
            rendered.contains(&tmp.0.join(CONFIG_FILE).display().to_string()),
            "error must name the path it tried: {rendered}"
        );
        assert!(matches!(err, ConfigError::IoError { .. }));
    }

    #[test]
    fn a_malformed_config_names_the_path_it_parsed() {
        let tmp = TmpDir::new("malformed");
        let dir = tmp.write_config("this is not toml = = =");
        let err = Config::from_path(dir).expect_err("malformed");

        let rendered = err.to_string();
        assert!(
            rendered.contains(&dir.join(CONFIG_FILE).display().to_string()),
            "error must name the file it parsed: {rendered}"
        );
        assert!(matches!(err, ConfigError::ParseError { .. }));
    }
}
