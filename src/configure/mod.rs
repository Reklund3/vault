//! `vault configure` — idempotent first-run setup. Provisions `~/.vault/`, seeds
//! a `vault.toml` template when absent, prints the Claude Code hook entry to add,
//! and reports backend readiness.
//!
//! This runs *before* a config exists (it's the command you run on a fresh
//! machine), so provisioning must NOT call `Config::load()` (which errors on a
//! missing toml). Paths come from `$HOME` directly via `config::vault_dir_path`,
//! mirroring the hook logger (`hook/log.rs`). It deliberately never edits
//! `~/.claude/settings.json` — the hook entry is printed for the user to merge.

use std::io::Write;
use std::path::Path;

use serde_json::json;
use thiserror::Error;

use crate::config::{self, Config};
use crate::util::fs::{harden_dir, harden_file};
use crate::util::probe::{mlx_reachable, tei_reachable};

/// Embedded default config. Seeded verbatim when `~/.vault/vault.toml` is absent.
const VAULT_TOML_TEMPLATE: &str = include_str!("vault.toml.template");

/// The config filename. `config::CONFIG_FILE` is private, and `seed_config`
/// takes an injectable dir (so tests can target a temp dir), so the name is
/// joined here rather than via a HOME-resolving config helper.
const TOML_FILE: &str = "vault.toml";

#[derive(Debug, Error)]
pub enum ConfigureError {
    #[error("could not resolve home directory (set HOME or USERPROFILE)")]
    HomeNotFound,
    #[error("io error during configure: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not resolve the running executable path: {0}")]
    Exe(std::io::Error),
}

pub struct ConfigureOptions {
    /// Re-seed `vault.toml` from the template even if one exists (clobbers a
    /// hand-authored file). Off by default — the safe path only writes when absent.
    pub force: bool,
}

/// What the `vault.toml` seeding step did.
#[derive(Debug, PartialEq, Eq)]
enum SeedOutcome {
    Wrote,
    Existed,
    Overwrote,
}

/// Entry point for `vault configure`. Composes the testable helpers below over
/// the real `$HOME`-derived paths and stdout.
pub fn run(opts: ConfigureOptions) -> Result<(), ConfigureError> {
    let dir = config::vault_dir_path().ok_or(ConfigureError::HomeNotFound)?;
    let exe = std::env::current_exe().map_err(ConfigureError::Exe)?;
    let mut out = std::io::stdout().lock();

    ensure_vault_dir(&dir)?;
    writeln!(out, "✓ ~/.vault/ ready ({})", dir.display())?;

    let toml_path = dir.join(TOML_FILE);
    match seed_config(&dir, opts.force)? {
        SeedOutcome::Wrote => writeln!(
            out,
            "✓ seeded {} — edit [mlx].router_model before syncing",
            toml_path.display()
        )?,
        SeedOutcome::Existed => writeln!(out, "• {} exists — left as-is", toml_path.display())?,
        SeedOutcome::Overwrote => writeln!(
            out,
            "⚠ overwrote {} from template (--force)",
            toml_path.display()
        )?,
    }

    writeln!(out)?;
    write!(out, "{}", render_hook_instructions(&exe))?;
    writeln!(out)?;
    report_readiness(&mut out)?;
    Ok(())
}

/// Create `~/.vault/` (idempotent) and harden it to `0700`.
fn ensure_vault_dir(dir: &Path) -> Result<(), ConfigureError> {
    std::fs::create_dir_all(dir)?;
    harden_dir(dir);
    Ok(())
}

/// Write the template to `<dir>/vault.toml`. With `force == false` an existing
/// file is left untouched (`Existed`); otherwise it's overwritten. Newly written
/// files are hardened to `0600`.
fn seed_config(dir: &Path, force: bool) -> Result<SeedOutcome, ConfigureError> {
    let path = dir.join(TOML_FILE);
    let existed = path.exists();
    if existed && !force {
        return Ok(SeedOutcome::Existed);
    }
    std::fs::write(&path, VAULT_TOML_TEMPLATE)?;
    harden_file(&path);
    Ok(if existed {
        SeedOutcome::Overwrote
    } else {
        SeedOutcome::Wrote
    })
}

/// The `~/.claude/settings.json` hook entry, in the authoritative nested shape
/// (`UserPromptSubmit` → `hooks` → `{ type: "command", command }`). Built via
/// `serde_json` so it is always valid JSON in the working form. With no `args`
/// field, Claude Code runs `command` as a shell string (`sh -c`), so the path is
/// **quoted** — an unquoted path containing a space would split and the hook would
/// silently never fire. Left un-canonicalized so a Homebrew symlink path stays
/// stable across upgrades.
fn hook_settings_json(exe: &Path) -> serde_json::Value {
    let command = format!("\"{}\" hook", exe.display());
    json!({
        "hooks": {
            "UserPromptSubmit": [
                { "hooks": [ { "type": "command", "command": command } ] }
            ]
        }
    })
}

fn render_hook_instructions(exe: &Path) -> String {
    let pretty =
        serde_json::to_string_pretty(&hook_settings_json(exe)).unwrap_or_else(|_| "{}".into());
    format!(
        "Register the hook — merge this into ~/.claude/settings.json (combine the\n\
         \"hooks\" block with any existing one; do not blindly overwrite):\n\n\
         {pretty}\n\n\
         Note: this is the absolute path of the binary you just ran. If you installed\n\
         via a symlink (e.g. Homebrew), prefer your stable PATH location (e.g.\n\
         /opt/homebrew/bin/vault) so the entry survives upgrades.\n"
    )
}

/// Print backend reachability and the `ANTHROPIC_API_KEY` presence, then next
/// steps. Endpoints come from a loaded config when one now exists, else the
/// built-in defaults — so this never hard-errors on a fresh machine.
fn report_readiness(out: &mut impl Write) -> Result<(), ConfigureError> {
    let config = Config::load().unwrap_or_default();
    let mlx = config.mlx_endpoint();
    let tei = config.embedding_endpoint();

    writeln!(out, "Readiness:")?;
    writeln!(
        out,
        "  MLX ({mlx}): {}",
        reachable_label(mlx_reachable(mlx))
    )?;
    writeln!(
        out,
        "  TEI ({tei}): {}",
        reachable_label(tei_reachable(tei))
    )?;
    // Presence only — never echo the key value (security rule). With MLX down,
    // the Haiku fallback is what keeps the router/classifier alive, and it needs
    // this set, so its presence makes the readiness output predictive.
    let key = if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        "set"
    } else {
        "not set (needed for the Haiku fallback when MLX is down)"
    };
    writeln!(out, "  ANTHROPIC_API_KEY: {key}")?;

    writeln!(out)?;
    writeln!(out, "Next steps:")?;
    writeln!(
        out,
        "  1. Edit ~/.vault/vault.toml ([mlx].router_model, endpoints)."
    )?;
    writeln!(out, "  2. Start embeddings:   vault tei start")?;
    writeln!(
        out,
        "  3. Add the hook entry above to ~/.claude/settings.json."
    )?;
    writeln!(out, "  4. Index a repo:       vault index sync <repo>")?;
    Ok(())
}

fn reachable_label(ok: bool) -> &'static str {
    if ok { "reachable" } else { "not reachable" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Unique temp dir under `std::env::temp_dir()`, cleaned on drop — mirrors the
    /// `TmpDir`/`Tmp` helpers in `hook/log.rs` and `index/walk.rs` (no tempfile dep).
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "vault-configure-{tag}-{}-{nanos}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn template_parses_as_current_config_schema() {
        // The seeded template must load under the live `Config` schema, or
        // `configure` would hand users a vault.toml that `Config::load` rejects.
        let cfg: Config = toml::from_str(VAULT_TOML_TEMPLATE).expect("template parses");
        assert_eq!(cfg.token_budget(), 10000);
        assert_eq!(cfg.router_timeout(), std::time::Duration::from_secs(3));
        // The seeded template defaults to the local-first Haiku fallback so a
        // fresh install behaves exactly as before the openai backend existed;
        // the openai fields are commented out until the user opts in.
        assert_eq!(cfg.router_remote(), "haiku");
        assert_eq!(cfg.classifier_remote(), "haiku");
    }

    #[test]
    fn seed_writes_template_when_absent() {
        let tmp = TmpDir::new("absent");
        assert_eq!(seed_config(tmp.path(), false).unwrap(), SeedOutcome::Wrote);
        let written = fs::read_to_string(tmp.path().join(TOML_FILE)).unwrap();
        assert_eq!(written, VAULT_TOML_TEMPLATE);
    }

    #[test]
    fn seed_does_not_clobber_existing() {
        let tmp = TmpDir::new("existing");
        let path = tmp.path().join(TOML_FILE);
        fs::write(&path, "# hand authored\n").unwrap();
        assert_eq!(
            seed_config(tmp.path(), false).unwrap(),
            SeedOutcome::Existed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "# hand authored\n");
    }

    #[test]
    fn seed_force_overwrites_existing() {
        let tmp = TmpDir::new("force");
        let path = tmp.path().join(TOML_FILE);
        fs::write(&path, "# hand authored\n").unwrap();
        assert_eq!(
            seed_config(tmp.path(), true).unwrap(),
            SeedOutcome::Overwrote
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), VAULT_TOML_TEMPLATE);
    }

    #[test]
    fn ensure_vault_dir_is_idempotent() {
        let tmp = TmpDir::new("ensure");
        let nested = tmp.path().join("sub");
        ensure_vault_dir(&nested).unwrap();
        ensure_vault_dir(&nested).unwrap(); // second call must not error
        assert!(nested.is_dir());
    }

    #[test]
    fn hook_json_has_nested_command_shape() {
        // Guards against regressing to the simplified (non-firing) schema.
        let v = hook_settings_json(Path::new("/usr/local/bin/vault"));
        let entry = &v["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(entry["type"], "command");
        // Shell-string form with the path quoted (handles spaces in the path).
        assert_eq!(entry["command"], "\"/usr/local/bin/vault\" hook");
    }
}
