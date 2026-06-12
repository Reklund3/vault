//! Hook outcome log — one JSONL record per `vault hook` invocation, appended
//! to `~/.vault/hook.log`. The hook always exits 0 with empty stdout on
//! failure, so this log is the only place that distinguishes "no relevant
//! context" from "the router has been down for a month".
//!
//! Metadata only: never the prompt, never chunk content. Error details are
//! truncated so the log can't become a transcript of model replies. Logging is
//! best-effort — every failure inside this module is swallowed, because
//! fail-open applies to the logger too.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::Outcome;
use crate::util::fs::{harden_dir, harden_file};

const LOG_FILE: &str = "hook.log";
const ROTATED_FILE: &str = "hook.log.1";
/// Rotate once the live log crosses this size. ~150 bytes/record means months
/// of history; live + rotated bound worst-case disk use at ~10MB.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Cap on stored error detail — long enough to identify a failure (transport
/// error, the head of an unparseable model reply), short enough that the log
/// never accumulates whole responses.
const MAX_DETAIL_CHARS: usize = 200;

/// Per-stage measurements collected as the pipeline runs. All optional — a
/// pipeline that fails at config-load never reaches the router.
#[derive(Debug, Default)]
pub(crate) struct Telemetry {
    pub backend: Option<&'static str>,
    pub router_ms: Option<u64>,
    pub embed_ms: Option<u64>,
    pub query_ms: Option<u64>,
}

/// One log line. `Option` fields serialize only when present, keeping records
/// compact and shape-stable per outcome kind.
#[derive(serde::Serialize)]
struct Record<'a> {
    ts: String,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    router_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_ms: Option<u64>,
    total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<u32>,
}

/// Append one record for this invocation. Failures here are deliberately
/// ignored — the hook must never fail (or block) because its own diagnostics
/// couldn't be written.
pub(crate) fn append_best_effort(outcome: &Outcome, tel: &Telemetry, total: Duration) {
    let Some(dir) = crate::config::vault_dir_path() else {
        return;
    };
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(line) = render_line(outcome, tel, total, now_secs) {
        append_to(&dir, &line, MAX_LOG_BYTES);
    }
}

fn render_line(
    outcome: &Outcome,
    tel: &Telemetry,
    total: Duration,
    now_secs: u64,
) -> Option<String> {
    let (kind, reason, stage, error, chunks, tokens) = match outcome {
        Outcome::Injected { chunks, tokens, .. } => {
            ("injected", None, None, None, Some(*chunks), Some(*tokens))
        }
        Outcome::Skip { reason } => ("skip", Some(reason.as_str()), None, None, None, None),
        Outcome::Failed { stage, detail } => (
            "error",
            None,
            Some(stage.as_str()),
            Some(detail.as_str()),
            None,
            None,
        ),
    };
    let record = Record {
        ts: iso8601_utc(now_secs),
        outcome: kind,
        reason,
        stage,
        error,
        backend: tel.backend,
        router_ms: tel.router_ms,
        embed_ms: tel.embed_ms,
        query_ms: tel.query_ms,
        total_ms: total.as_millis() as u64,
        chunks,
        tokens,
    };
    serde_json::to_string(&record).ok().map(|mut s| {
        s.push('\n');
        s
    })
}

/// Append `line` to `<dir>/hook.log`, rotating to `hook.log.1` once the live
/// file crosses `max_bytes`. Path resolution stays in the caller so tests can
/// point this at a temp dir.
fn append_to(dir: &Path, line: &str, max_bytes: u64) {
    let _ = fs::create_dir_all(dir);
    harden_dir(dir);
    let path = dir.join(LOG_FILE);
    let oversized = fs::metadata(&path)
        .map(|m| m.len() > max_bytes)
        .unwrap_or(false);
    if oversized {
        let rotated = dir.join(ROTATED_FILE);
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, &rotated);
    }
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = f.write_all(line.as_bytes());
    harden_file(&path);
}

/// Truncate error detail to `MAX_DETAIL_CHARS` characters (char-boundary
/// safe), appending an ellipsis when anything was cut.
pub(crate) fn truncate_detail(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_DETAIL_CHARS).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// Format epoch seconds as `YYYY-MM-DDTHH:MM:SSZ` without a date dependency.
/// Days-to-civil per Howard Hinnant's algorithm.
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{SkipReason, Stage};
    use std::path::PathBuf;

    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("vault-hooklog-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn iso8601_epoch_zero() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_known_recent_date() {
        assert_eq!(iso8601_utc(1_767_225_600), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_leap_day() {
        assert_eq!(iso8601_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn iso8601_time_of_day() {
        assert_eq!(iso8601_utc(1_767_225_600 + 3661), "2026-01-01T01:01:01Z");
    }

    #[test]
    fn truncate_short_detail_is_unchanged() {
        assert_eq!(truncate_detail("transport: timeout"), "transport: timeout");
    }

    #[test]
    fn truncate_long_detail_caps_and_marks() {
        let long = "x".repeat(500);
        let t = truncate_detail(&long);
        assert_eq!(t.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_respects_multibyte_boundaries() {
        let long = "é".repeat(300);
        let t = truncate_detail(&long);
        assert_eq!(t.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn render_line_injected_shape() {
        let outcome = Outcome::Injected {
            block: "<secret-content>".into(),
            chunks: 3,
            tokens: 1200,
        };
        let tel = Telemetry {
            backend: Some("gemma"),
            router_ms: Some(120),
            embed_ms: Some(40),
            query_ms: Some(8),
        };
        let line = render_line(&outcome, &tel, Duration::from_millis(180), 1_767_225_600).unwrap();
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["ts"], "2026-01-01T00:00:00Z");
        assert_eq!(v["outcome"], "injected");
        assert_eq!(v["backend"], "gemma");
        assert_eq!(v["router_ms"], 120);
        assert_eq!(v["total_ms"], 180);
        assert_eq!(v["chunks"], 3);
        assert_eq!(v["tokens"], 1200);
        assert!(v.get("stage").is_none());
        assert!(v.get("error").is_none());
        assert!(v.get("reason").is_none());
        // The rendered block must never reach the log.
        assert!(!line.contains("secret-content"));
    }

    #[test]
    fn render_line_error_shape() {
        let outcome = Outcome::Failed {
            stage: Stage::RouterPlan,
            detail: "transport: timed out".into(),
        };
        let tel = Telemetry {
            backend: Some("haiku"),
            router_ms: Some(3000),
            ..Default::default()
        };
        let line = render_line(&outcome, &tel, Duration::from_millis(3001), 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["outcome"], "error");
        assert_eq!(v["stage"], "router-plan");
        assert_eq!(v["error"], "transport: timed out");
        assert_eq!(v["backend"], "haiku");
        assert_eq!(v["router_ms"], 3000);
        assert!(v.get("embed_ms").is_none());
        assert!(v.get("chunks").is_none());
    }

    #[test]
    fn render_line_skip_shape() {
        let outcome = Outcome::Skip {
            reason: SkipReason::RouterSkip,
        };
        let tel = Telemetry {
            backend: Some("gemma"),
            router_ms: Some(95),
            ..Default::default()
        };
        let line = render_line(&outcome, &tel, Duration::from_millis(96), 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["outcome"], "skip");
        assert_eq!(v["reason"], "router-skip");
        assert!(v.get("stage").is_none());
        assert!(v.get("error").is_none());
    }

    #[test]
    fn append_creates_and_appends() {
        let tmp = TmpDir::new("append");
        append_to(&tmp.0, "{\"a\":1}\n", 1024);
        append_to(&tmp.0, "{\"b\":2}\n", 1024);
        let content = fs::read_to_string(tmp.0.join(LOG_FILE)).unwrap();
        assert_eq!(content, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn append_rotates_past_cap() {
        let tmp = TmpDir::new("rotate");
        let big = "x".repeat(64);
        append_to(&tmp.0, &big, 16); // empty file → no rotation, writes 64 bytes
        append_to(&tmp.0, "fresh\n", 16); // 64 > 16 → rotate, then write
        assert_eq!(fs::read_to_string(tmp.0.join(ROTATED_FILE)).unwrap(), big);
        assert_eq!(fs::read_to_string(tmp.0.join(LOG_FILE)).unwrap(), "fresh\n");
    }

    #[test]
    fn append_rotation_replaces_prior_rotated_file() {
        let tmp = TmpDir::new("rotate2");
        fs::write(tmp.0.join(ROTATED_FILE), "ancient\n").unwrap();
        let big = "y".repeat(64);
        append_to(&tmp.0, &big, 16);
        append_to(&tmp.0, "fresh\n", 16);
        assert_eq!(fs::read_to_string(tmp.0.join(ROTATED_FILE)).unwrap(), big);
        assert_eq!(fs::read_to_string(tmp.0.join(LOG_FILE)).unwrap(), "fresh\n");
    }
}
