use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Config, ConfigError};
use crate::util::fs::{harden_dir, harden_file};
use crate::util::probe::tei_reachable;

const PID_FILE: &str = "tei.pid";
const LOG_FILE: &str = "tei.log";

/// How long `start` polls for the port to answer before returning with a
/// "started but not answering yet" note. First-run weight downloads can exceed
/// this; the process keeps running regardless — this only bounds how long we
/// block the terminal waiting to confirm readiness.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Last N lines `vault tei logs` prints.
const LOG_TAIL_LINES: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum LauncherError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error(
        "no launcher_cmd configured — set [embeddings].launcher_cmd in vault.toml, e.g.\n  \
         launcher_cmd = \"text-embeddings-router --model-id {model} --port 8081\"\n\
         or start TEI manually."
    )]
    NoLauncherCmd { model: String },

    #[error("launcher_cmd is malformed: {0}")]
    BadLauncherCmd(String),

    #[error("could not run {program:?} (from launcher_cmd): {source}")]
    Spawn { program: String, source: io::Error },

    #[error("invalid pidfile {path}: {detail}")]
    BadPidFile { path: String, detail: String },
}

// ----------------------------------------------------------------------------
// start
// ----------------------------------------------------------------------------

pub(crate) fn start(config: &Config) -> Result<(), LauncherError> {
    let endpoint = config.embedding_endpoint();
    if tei_reachable(endpoint) {
        println!("TEI already reachable on {endpoint} — nothing to do.");
        return Ok(());
    }

    let raw = config
        .embedding_launcher_cmd()
        .ok_or_else(|| LauncherError::NoLauncherCmd {
            model: config.embedding_model().to_string(),
        })?;
    let tokens = split_command(raw)?;
    let (program, args) = tokens
        .split_first()
        .ok_or_else(|| LauncherError::BadLauncherCmd("empty command".to_string()))?;

    let vault_dir = config.vault_dir()?;
    fs::create_dir_all(&vault_dir)?;
    harden_dir(&vault_dir);

    let log_path = vault_dir.join(LOG_FILE);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    harden_file(&log_path);
    let log_err = log.try_clone()?;

    let mut command = Command::new(program);
    command.args(args);
    scrub_env(&mut command);
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(log));
    command.stderr(Stdio::from(log_err));
    detach(&mut command);

    let child = command.spawn().map_err(|source| LauncherError::Spawn {
        program: program.clone(),
        source,
    })?;
    let pid = child.id();
    // Intentionally do NOT wait on `child`: it is detached and must outlive us.
    // Dropping the handle leaks it; the OS reparents the process when we exit.

    let pid_path = vault_dir.join(PID_FILE);
    fs::write(&pid_path, pid.to_string())?;
    harden_file(&pid_path);

    println!(
        "Started TEI (pid {pid}). Logging to {}.",
        log_path.display()
    );

    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if tei_reachable(endpoint) {
            println!("TEI is reachable on {endpoint}.");
            return Ok(());
        }
        std::thread::sleep(READY_POLL_INTERVAL);
    }

    println!(
        "TEI process is running but {endpoint} is not answering yet.\n\
         First run downloads model weights and can take minutes — watch `vault tei logs`,\n\
         then confirm with `vault tei status`."
    );
    Ok(())
}

// ----------------------------------------------------------------------------
// stop
// ----------------------------------------------------------------------------

pub(crate) fn stop(config: &Config) -> Result<(), LauncherError> {
    let pid_path = config.vault_dir()?.join(PID_FILE);
    let pid = match read_pid(&pid_path)? {
        Some(pid) => pid,
        None => {
            println!(
                "No pidfile at {} — TEI was not started by vault (or is already stopped).",
                pid_path.display()
            );
            return Ok(());
        }
    };

    // Best-effort: the process may already be gone. Either way, clear the
    // pidfile so a stale pid doesn't linger.
    let killed = kill(pid);
    let _ = fs::remove_file(&pid_path);
    if killed {
        println!("Stopped TEI (pid {pid}).");
    } else {
        println!("TEI (pid {pid}) was not running; cleared stale pidfile.");
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// status
// ----------------------------------------------------------------------------

pub(crate) fn status(config: &Config) -> Result<(), LauncherError> {
    let endpoint = config.embedding_endpoint();
    let reachable = tei_reachable(endpoint);
    let pid_path = config.vault_dir()?.join(PID_FILE);
    let pid = read_pid(&pid_path)?;

    println!("endpoint:  {endpoint}");
    println!("reachable: {}", if reachable { "yes" } else { "no" });
    match pid {
        Some(p) => println!("pidfile:   {} (pid {p})", pid_path.display()),
        None => println!("pidfile:   none ({})", pid_path.display()),
    }
    match config.embedding_launcher_cmd() {
        Some(cmd) => println!("launcher:  {cmd}"),
        None => println!("launcher:  (unset — `vault tei start` will error)"),
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// logs
// ----------------------------------------------------------------------------

pub(crate) fn logs(config: &Config) -> Result<(), LauncherError> {
    let log_path = config.vault_dir()?.join(LOG_FILE);
    let contents = match fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!(
                "No log file at {} — has `vault tei start` run yet?",
                log_path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    if start > 0 {
        println!(
            "# {} (last {LOG_TAIL_LINES} of {} lines)",
            log_path.display(),
            lines.len()
        );
    } else {
        println!("# {}", log_path.display());
    }
    for line in &lines[start..] {
        println!("{line}");
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

/// Read and parse the pidfile. `Ok(None)` when it doesn't exist; a present but
/// unparseable file is an error so the user notices corruption rather than
/// silently leaking a process.
fn read_pid(pid_path: &Path) -> Result<Option<u32>, LauncherError> {
    match fs::read_to_string(pid_path) {
        Ok(s) => {
            let trimmed = s.trim();
            let pid = trimmed
                .parse::<u32>()
                .map_err(|e| LauncherError::BadPidFile {
                    path: pid_path.display().to_string(),
                    detail: format!("{trimmed:?}: {e}"),
                })?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Split a launcher command into program + args, honoring double quotes so paths
/// with spaces survive (`"C:\Program Files\tei\router.exe" --port 8081`). This
/// is a pragmatic splitter, not a full shell parser: it understands double
/// quotes only — enough for model ids, ports, and quoted Windows paths.
fn split_command(input: &str) -> Result<Vec<String>, LauncherError> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true; // `""` is a real (empty) token
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }

    if in_quotes {
        return Err(LauncherError::BadLauncherCmd(
            "unbalanced quotes".to_string(),
        ));
    }
    if has_token {
        tokens.push(cur);
    }
    if tokens.is_empty() {
        return Err(LauncherError::BadLauncherCmd("empty command".to_string()));
    }
    Ok(tokens)
}

/// Scrub the child environment to a minimal allowlist. The security rule is
/// `ANTHROPIC_API_KEY` (and anything else unrelated) must never reach TEI;
/// see `docs/security.md` → "Secrets and credentials". We `env_clear()` then
/// re-add only what TEI needs to find its binary, model cache, and locale.
/// Windows additionally needs a handful of system vars or the process won't
/// start — these are not secrets.
fn scrub_env(command: &mut Command) {
    command.env_clear();

    // Cross-platform: PATH to find the binary, HOME for default cache roots,
    // the HuggingFace cache vars, and locale.
    const PASSTHROUGH: &[&str] = &[
        "PATH",
        "HOME",
        "HF_HUB_CACHE",
        "HF_HOME",
        "HUGGINGFACE_HUB_CACHE",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
    ];
    for key in PASSTHROUGH {
        if let Some(val) = std::env::var_os(key) {
            command.env(key, val);
        }
    }

    // Windows: these are required for a process to even start (DLL resolution,
    // temp dirs, user profile). Not secrets; omitting them breaks the spawn.
    #[cfg(windows)]
    {
        const WIN_PASSTHROUGH: &[&str] = &[
            "SystemRoot",
            "windir",
            "SystemDrive",
            "USERPROFILE",
            "TEMP",
            "TMP",
            "APPDATA",
            "LOCALAPPDATA",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "USERNAME",
            "COMPUTERNAME",
        ];
        for key in WIN_PASSTHROUGH {
            if let Some(val) = std::env::var_os(key) {
                command.env(key, val);
            }
        }
    }
}

/// Detach the child so it survives the parent shell exiting and isn't killed by
/// a Ctrl-C aimed at vault. Unix: `process_group(0)` puts it in its own group
/// (stable, no libc dependency). Windows: `DETACHED_PROCESS` drops the console
/// and `CREATE_NEW_PROCESS_GROUP` isolates it from our Ctrl-C.
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

/// Terminate a pid. Dep-free by shelling out to the platform's own tool rather
/// than pulling in `libc` / `windows` for one call. Returns whether the kill
/// reported success — a `false` typically means the process was already gone,
/// which `stop` treats as "clear the stale pidfile".
#[cfg(unix)]
fn kill(pid: u32) -> bool {
    Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn kill(pid: u32) -> bool {
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn kill(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_simple() {
        let t = split_command("text-embeddings-router --port 8081").unwrap();
        assert_eq!(t, ["text-embeddings-router", "--port", "8081"]);
    }

    #[test]
    fn split_command_collapses_runs_of_whitespace() {
        let t = split_command("  a   b\tc  ").unwrap();
        assert_eq!(t, ["a", "b", "c"]);
    }

    #[test]
    fn split_command_honors_double_quotes() {
        let t = split_command("\"C:\\Program Files\\tei\\router.exe\" --port 8081").unwrap();
        assert_eq!(t, ["C:\\Program Files\\tei\\router.exe", "--port", "8081"]);
    }

    #[test]
    fn split_command_quote_in_middle_joins() {
        // --model-id="a b" → single arg `--model-id=a b`
        let t = split_command("router --model-id=\"a b\"").unwrap();
        assert_eq!(t, ["router", "--model-id=a b"]);
    }

    #[test]
    fn split_command_unbalanced_quotes_is_error() {
        let err = split_command("router \"unterminated").unwrap_err();
        assert!(matches!(err, LauncherError::BadLauncherCmd(_)));
    }

    #[test]
    fn split_command_empty_is_error() {
        assert!(matches!(
            split_command("   ").unwrap_err(),
            LauncherError::BadLauncherCmd(_)
        ));
    }

    #[test]
    fn read_pid_missing_file_is_none() {
        let dir = std::env::temp_dir().join("vault-tei-test-missing");
        let path = dir.join("nope.pid");
        assert!(read_pid(&path).unwrap().is_none());
    }

    #[test]
    fn read_pid_parses_value() {
        let path = std::env::temp_dir().join(format!("vault-tei-test-{}.pid", std::process::id()));
        fs::write(&path, "4242\n").unwrap();
        assert_eq!(read_pid(&path).unwrap(), Some(4242));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_pid_garbage_is_error() {
        let path =
            std::env::temp_dir().join(format!("vault-tei-test-bad-{}.pid", std::process::id()));
        fs::write(&path, "not-a-pid").unwrap();
        assert!(matches!(
            read_pid(&path).unwrap_err(),
            LauncherError::BadPidFile { .. }
        ));
        let _ = fs::remove_file(&path);
    }
}
