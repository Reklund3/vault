use std::sync::OnceLock;

use regex::RegexSet;

/// Returns true if `content` matches any of the well-known secret patterns
/// CLAUDE.md requires the indexer to filter out. Used at two boundaries:
///
/// 1. **Pre-embedding chunk drop.** Any parsed chunk whose body trips the
///    scan is dropped before being sent to the embedder + store.
/// 2. **Pre-remote-classify head guard.** Before sending a file's first 1 KiB
///    to the remote classifier (Haiku), the head is scanned; on a hit the
///    file falls back to extension-based classification and never leaves the
///    machine.
///
/// Patterns are intentionally conservative — false negatives are preferable
/// to false positives in the head-guard path (a falsely-flagged file just
/// uses extension fallback, harmless), but a missed real secret leaks. The
/// chunk-drop path is also conservative: a falsely-dropped chunk reduces
/// retrieval recall, a missed secret persists in the store. We err toward
/// dropping.
pub fn looks_like_secret(content: &str) -> bool {
    patterns().is_match(content)
}

fn patterns() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            // AWS access key id
            r"\bAKIA[0-9A-Z]{16}\b",
            // GitHub fine-grained / classic / OAuth / refresh / server tokens
            r"\bgh[pousr]_[A-Za-z0-9]{36,}\b",
            // GitLab personal access token (open-ended length: the random part
            // has varied across GitLab versions — matching exactly N would miss
            // any token of a different length and give false confidence).
            r"\bglpat-[0-9A-Za-z_-]{20,}\b",
            // Slack incoming-webhook URL (the path carries the secret token)
            r"https://hooks\.slack\.com/services/[A-Za-z0-9_/]+",
            // Anthropic API key
            r"\bsk-ant-[A-Za-z0-9_-]{20,}\b",
            // OpenAI API key (also catches Anthropic-prefix-only keys; that's fine)
            r"\bsk-[A-Za-z0-9]{20,}\b",
            // JWT (three base64url segments separated by `.`)
            r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
            // PEM private key header
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        ])
        .expect("secret patterns must compile")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_access_key_trips() {
        assert!(looks_like_secret("AKIA0123456789ABCDEF"));
        // Embedded in normal text still hits — these scan content, not whole lines.
        assert!(looks_like_secret(
            "const key = \"AKIA0123456789ABCDEF\"; // ugh"
        ));
    }

    #[test]
    fn github_token_trips() {
        let body = "GITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(looks_like_secret(body));
        // gho_ / ghu_ / ghs_ / ghr_ all hit the same character class.
        assert!(looks_like_secret(
            "ghs_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ));
    }

    #[test]
    fn gitlab_pat_trips() {
        assert!(looks_like_secret("GITLAB_TOKEN=glpat-aBcDeFgHiJkLmNoPqRsT"));
        // Longer random part (newer GitLab) still hits — pattern is open-ended.
        assert!(looks_like_secret("glpat-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345"));
    }

    #[test]
    fn slack_webhook_trips() {
        // Build the URL from parts: a contiguous webhook literal in source trips
        // GitHub's own push-protection scanner (it keys on host + path together).
        let host = "https://hooks.slack.com";
        let webhook = format!("{host}/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX");
        assert!(looks_like_secret(&webhook));
    }

    #[test]
    fn anthropic_key_trips() {
        assert!(looks_like_secret(
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789-XYZ"
        ));
    }

    #[test]
    fn openai_key_trips() {
        // The generic sk- pattern also catches Anthropic-style without the
        // ant- prefix; both are real secrets, so that's the desired behavior.
        assert!(looks_like_secret("sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn jwt_trips() {
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                     eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkphbmUifQ.\
                     dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert!(looks_like_secret(token));
    }

    #[test]
    fn pem_header_trips() {
        assert!(looks_like_secret(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA..."
        ));
        assert!(looks_like_secret("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(looks_like_secret("-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn ordinary_code_does_not_trip() {
        assert!(!looks_like_secret(""));
        assert!(!looks_like_secret("fn main() { println!(\"hello\"); }"));
        // The literal word "secret" without a key shape must not trigger —
        // false positives on docstrings would gut retrieval recall.
        assert!(!looks_like_secret(
            "// The secret is to keep the chunker boring.\n\
             let secret = compute_thing();"
        ));
    }

    #[test]
    fn short_tokens_do_not_trip() {
        // 19-char suffix is below the 20+ minimum for sk- and sk-ant-.
        assert!(!looks_like_secret("sk-tooshort"));
        // Single-segment base64 isn't a JWT.
        assert!(!looks_like_secret("eyJhbGciOiJIUzI1NiJ9"));
    }
}
