//! Shared fixtures for the black-box integration tests.
//!
//! `tests/common/mod.rs` rather than `tests/common.rs` on purpose: cargo builds
//! every top-level file in `tests/` as its own test binary, and this one holds
//! no tests.

use std::path::{Path, PathBuf};

use vault::{Config, PlannedQuery, QueryPlan};

/// One temp dir per test; removed on drop.
pub struct TmpDir(PathBuf);

impl TmpDir {
    pub fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("vault-it-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        Self(root)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn write(&self, rel: &str, body: &str) {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, body).expect("write");
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A config whose vault directory is the given path. This is Step 7's
/// `with_vault_dir` doing exactly what it was added for — without it, every test
/// here would write to the developer's real `~/.vault/vault.db`.
pub fn config_in(dir: &Path) -> Config {
    Config::default().with_vault_dir(dir)
}

/// A config whose router is pinned to local Gemma, written to a real
/// `vault.toml` and loaded back through the public `Config::from_path`.
///
/// `config_in` leaves the router on its `auto` default, which probes
/// `localhost:8080` and then needs `ANTHROPIC_API_KEY` — so on a machine with
/// neither (CI, most laptops) `QueryPlanner::new` returns `Err` and any test
/// built on it can only skip. Forcing `gemma` mode makes construction
/// deterministic everywhere: `build_router` goes straight to
/// `GemmaRouter::from_config`, which needs a parseable endpoint and a model
/// name and makes no network call.
///
/// The endpoints point at port 1 on purpose. Nothing here should ever send a
/// request; if something does, it fails loudly instead of quietly reaching a
/// service the developer happens to be running.
///
/// `allow(dead_code)` because cargo compiles this module separately into every
/// test binary that declares `mod common;`, and only `pipeline.rs` needs this
/// one — an unused-function error in `no_stdout.rs` would otherwise be the
/// result. The other helpers here happen to be used by both.
#[allow(dead_code)]
pub fn offline_config_in(dir: &Path) -> Config {
    std::fs::write(
        dir.join("vault.toml"),
        r#"
[defaults]
context_tag  = "vault-context"
token_budget = 10000
alpha        = 0.6
min_score    = 0.15

[router]
mode  = "gemma"
model = "unused-in-gemma-mode"

[mlx]
endpoint     = "http://127.0.0.1:1"
router_model = "test-model"

[embeddings]
endpoint = "http://127.0.0.1:1"
model    = "nomic-ai/nomic-embed-text-v1.5"
dims     = 768
"#,
    )
    .expect("write vault.toml");
    Config::from_path(dir).expect("config loads")
}

/// A plan a consumer can build without a router or an embedder. The embedding is
/// zeroed rather than random: cosine scores are irrelevant to these tests, and a
/// fixed vector keeps them deterministic.
pub fn plan_for(config: &Config, projects: Vec<String>) -> PlannedQuery {
    PlannedQuery {
        plan: QueryPlan {
            projects,
            type_names: vec![],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        },
        embedding: vec![0.0; config.embedding_dim()],
    }
}
