use thiserror::Error;
use std::path::PathBuf;

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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    defaults: Defaults,
    router: Router,
    mlx: Mlx,
    embeddings: Embeddings,
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

    pub fn alpha(&self) -> f32 {
        self.defaults.alpha
    }

    pub fn token_budget(&self) -> u16 {
        self.defaults.token_budget
    }

    pub fn min_score(&self) -> f32 {
        self.defaults.min_score
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
            },
            mlx: Mlx {
                endpoint: "http://localhost:8080".to_string(),
                router_model: "gemma-4-31b-bf16".to_string(),
            },
            embeddings: Embeddings {
                endpoint: "http://localhost:8081".to_string(),
                model: "nomic-ai/nomic-embed-text-v1.5".to_string(),
                dims: 768,
            },
        }
    }
}
