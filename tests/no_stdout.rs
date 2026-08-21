//! Design rule 3 — the library never writes to stdout — in a test binary of its
//! own.
//!
//! This matters for a future stdio MCP server, where stdout *is* the JSON-RPC
//! channel: a stray `println!` in pipeline code would not read as a bug, it
//! would read as a protocol violation, and it would be miserable to trace.
//!
//! Step 8 put the four printing modules (`hook`, `diagnose`, `configure`,
//! `tei`) behind the `cli` feature, so a consumer who opts out cannot *reach* a
//! printing entry point. This file covers the other half — that the pipelines a
//! consumer does reach stay silent — which only a dynamic check can do.
//!
//! ## Why this is so much machinery
//!
//! Two things fight a naive version of this test, and both were found the hard
//! way:
//!
//! 1. **`cargo test` captures Rust-level output.** The default harness swaps out
//!    `print!`'s destination, so `println!` never reaches file descriptor 1 at
//!    all. Redirecting fd 1 in-process therefore sees nothing, and the test
//!    passes no matter what the library prints. The fix is `--nocapture`, which
//!    cannot be set from inside a test — hence a child process.
//! 2. **The harness writes its own progress lines to fd 1.** "running 1 test",
//!    "test foo ... ok". Those must not land in the capture, so the child
//!    redirects fd 1 only around the library calls and puts it back afterwards;
//!    harness output goes to the pipe the parent is holding instead.
//!
//! So: the parent re-runs this binary with `--nocapture`, the child does the
//! work with fd 1 pointed at a file, and the parent reads that file back. Keep
//! this file to one test — fd 1 is process-wide, and a parallel neighbour would
//! contaminate the capture.

mod common;

use common::{TmpDir, config_in, plan_for};
use vault::{Interaction, SyncOptions, VaultStore};

/// Env var carrying the capture path. Its presence is also what tells a process
/// it is the child.
const CAPTURE_VAR: &str = "VAULT_RULE3_CAPTURE";
const TEST_NAME: &str = "the_library_writes_nothing_to_stdout";

#[cfg(unix)]
#[test]
fn the_library_writes_nothing_to_stdout() {
    match std::env::var(CAPTURE_VAR) {
        Ok(path) => run_library_with_stdout_captured(path.as_ref()),
        Err(_) => spawn_child_and_check_capture(),
    }
}

/// Parent half: re-run this binary for this test alone, with output capture
/// disabled so `println!` actually reaches fd 1.
#[cfg(unix)]
fn spawn_child_and_check_capture() {
    let tmp = TmpDir::new("rule3");
    let capture_path = tmp.path().join("stdout.capture");

    let exe = std::env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CAPTURE_VAR, &capture_path)
        .output()
        .expect("re-run this test binary");

    assert!(
        output.status.success(),
        "child run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let written = std::fs::read(&capture_path).expect("child must write a capture file");

    assert!(
        written.is_empty(),
        "the library wrote {} bytes to stdout, which would corrupt a stdio \
         protocol channel:\n{}",
        written.len(),
        String::from_utf8_lossy(&written)
    );
}

/// Child half: point fd 1 at `capture_path`, exercise every stage a consumer
/// touches without the CLI, then put fd 1 back.
///
/// Redirecting the real descriptor rather than Rust's `print!` machinery is
/// deliberate — it also catches a write from a C dependency, and SQLite and its
/// extensions are right there.
#[cfg(unix)]
fn run_library_with_stdout_captured(capture_path: &std::path::Path) {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let capture = std::fs::File::create(capture_path).expect("capture file");

    // SAFETY: plain descriptor bookkeeping. `saved` is closed below and fd 1 is
    // restored before this function returns; no Rust invariants are involved.
    let saved = unsafe { libc::dup(1) };
    assert!(saved >= 0, "dup(1) failed");
    assert!(
        unsafe { libc::dup2(capture.as_raw_fd(), 1) } >= 0,
        "dup2 onto stdout failed"
    );

    let result = exercise_library();

    // Flush what Rust buffered, then restore before asserting — a failure
    // message is worthless if it cannot be printed.
    let _ = std::io::stdout().flush();
    // SAFETY: as above.
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
    }

    result.expect("library calls must succeed");
}

/// Open a store, run a search, run a dry-run sync. No CLI module is named.
fn exercise_library() -> Result<(), vault::VaultError> {
    let vault_dir = TmpDir::new("rule3-vault");
    let repo = TmpDir::new("rule3-repo");
    repo.write("a.proto", "syntax = \"proto3\";\nmessage A {}\n");

    let config = config_in(vault_dir.path());
    let mut store = VaultStore::open(&config)?;
    store.search(&plan_for(&config, vec!["vault".to_string()]))?;
    store.sync(SyncOptions {
        repo: repo.path().to_path_buf(),
        explicit_name: Some("silent".to_string()),
        explicit_domain: None,
        dry_run: true,
        interaction: Interaction::NonInteractive {
            allow_remote_billing: false,
        },
    })?;
    Ok(())
}
