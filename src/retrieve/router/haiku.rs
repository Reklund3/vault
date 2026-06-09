use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::retrieve::RouterOutput;
use crate::retrieve::router::{
    ROUTER_SYSTEM, Router, RouterError, build_user_prompt, parse_response,
};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
// Timeout is configurable via [router].timeout_secs (default 3s per CLAUDE.md).
// The hook caller silences failures (passthrough), so the router's only job is
// to fail inside whatever budget the user configured.
/// Five small arrays + the skip shortcut; 256 covers either shape comfortably.
const MAX_TOKENS: u32 = 256;

/// Anthropic Haiku router. The API key is held only in memory and only ever
/// sent in the `x-api-key` header — never logged, never placed in an error
/// string, never read from any file vault writes. `Debug` is intentionally not
/// derived so the key can't leak through a debug print.
pub(crate) struct HaikuRouter {
    model: String,
    api_key: String,
    http: Client,
}

impl HaikuRouter {
    pub(crate) fn from_config(config: &Config) -> Result<Self, RouterError> {
        Self::from_config_with_timeout(config, config.router_timeout())
    }

    pub(crate) fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, RouterError> {
        let api_key = require_api_key(std::env::var("ANTHROPIC_API_KEY").ok())?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| RouterError::Transport(e.to_string()))?;
        Ok(Self {
            model: resolve_model(config.router_model()),
            api_key,
            http,
        })
    }
}

impl Router for HaikuRouter {
    fn plan(&self, prompt: &str) -> Result<RouterOutput, RouterError> {
        let user = build_user_prompt(prompt);
        let request = MessagesRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            // The system block carries the byte-identical ROUTER_SYSTEM behind
            // an ephemeral cache so only the per-prompt user turn is fresh.
            system: vec![SystemBlock {
                kind: "text",
                text: ROUTER_SYSTEM,
                cache_control: CacheControl { kind: "ephemeral" },
            }],
            messages: vec![UserMessage {
                role: "user",
                content: &user,
            }],
        };

        let resp = self
            .http
            .post(ANTHROPIC_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&request)
            .send()
            .map_err(|e| RouterError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            // Anthropic error bodies never echo the request key.
            let body = resp.text().unwrap_or_default();
            return Err(RouterError::BadResponse(format!("HTTP {status}: {body}")));
        }

        let body: MessagesResponse = resp
            .json()
            .map_err(|e| RouterError::BadResponse(e.to_string()))?;
        let text = body
            .content
            .into_iter()
            .next()
            .ok_or_else(|| RouterError::BadResponse("empty content".to_string()))?
            .text;

        parse_response(&text)
    }
}

fn require_api_key(value: Option<String>) -> Result<String, RouterError> {
    match value {
        Some(key) if !key.trim().is_empty() => Ok(key),
        _ => Err(RouterError::MissingApiKey),
    }
}

fn resolve_model(configured: &str) -> String {
    match configured {
        // Alias → current latest Haiku. Update this ID when a newer Haiku ships
        // (grep "claude-haiku" to find it).
        "haiku" => "claude-haiku-4-5-20251001".to_string(),
        other => other.to_string(),
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock<'a>>,
    messages: Vec<UserMessage<'a>>,
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
    cache_control: CacheControl,
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DocType, Language};

    #[test]
    fn resolve_model_maps_alias_and_passes_through() {
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5-20251001");
        assert_eq!(resolve_model("claude-custom-id"), "claude-custom-id");
    }

    #[test]
    fn require_api_key_rejects_missing_and_blank() {
        assert!(matches!(
            require_api_key(None),
            Err(RouterError::MissingApiKey)
        ));
        assert!(matches!(
            require_api_key(Some(String::new())),
            Err(RouterError::MissingApiKey)
        ));
        assert!(matches!(
            require_api_key(Some("   ".to_string())),
            Err(RouterError::MissingApiKey)
        ));
        assert_eq!(
            require_api_key(Some("sk-test".to_string())).unwrap(),
            "sk-test"
        );
    }

    #[test]
    fn deserializes_messages_plan_response() {
        let raw = r#"{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "{\"projects\":[\"vault\"],\"doc_types\":[\"convention\"],\"languages\":[\"rust\"]}"}],
            "stop_reason": "end_turn"
        }"#;
        let body: MessagesResponse = serde_json::from_str(raw).expect("deserialize");
        let text = body.content.into_iter().next().unwrap().text;
        let out = parse_response(&text).expect("parse");
        match out {
            RouterOutput::Plan(plan) => {
                assert_eq!(plan.projects, vec!["vault"]);
                assert_eq!(plan.doc_types, vec![DocType::Convention]);
                assert_eq!(plan.languages, vec![Language::Rust]);
            }
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn deserializes_messages_skip_response() {
        let raw = r#"{
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "{ \"skip\": true }"}],
            "stop_reason": "end_turn"
        }"#;
        let body: MessagesResponse = serde_json::from_str(raw).expect("deserialize");
        let text = body.content.into_iter().next().unwrap().text;
        let out = parse_response(&text).expect("parse");
        assert!(matches!(out, RouterOutput::Skip));
    }

    #[test]
    fn empty_content_is_bad_response() {
        let raw = r#"{"id":"m","type":"message","role":"assistant","content":[]}"#;
        let body: MessagesResponse = serde_json::from_str(raw).unwrap();
        let result = body
            .content
            .into_iter()
            .next()
            .ok_or_else(|| RouterError::BadResponse("empty content".to_string()))
            .map(|b| b.text);
        assert!(matches!(result, Err(RouterError::BadResponse(_))));
    }

    #[test]
    fn non_text_first_block_yields_bad_response() {
        // A tool_use (or other non-text) first block deserializes with an empty
        // `text`, and an empty reply has no JSON object → BadResponse.
        let raw = r#"{"content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]}"#;
        let body: MessagesResponse = serde_json::from_str(raw).unwrap();
        let text = body.content.into_iter().next().unwrap().text;
        assert!(matches!(
            parse_response(&text),
            Err(RouterError::BadResponse(_))
        ));
    }

    /// 30s for live tests: production sets the budget via
    /// `[router].timeout_secs` in `vault.toml`. Haiku usually replies in <2s so
    /// this is mostly headroom for network jitter.
    const LIVE_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    #[ignore = "requires ANTHROPIC_API_KEY and network"]
    fn live_haiku_route_plan() {
        let config = Config::default();
        let router = HaikuRouter::from_config_with_timeout(&config, LIVE_TIMEOUT).expect("client");
        let out = router
            .plan("How does the BuildRequest proto handle retries?")
            .expect("plan");
        let _ = out;
    }

    #[test]
    #[ignore = "requires ANTHROPIC_API_KEY and network"]
    fn live_haiku_route_skip() {
        // Skip is the zero-cost-passthrough optimization for the hook. ROUTER_SYSTEM
        // shows the skip shape as `{ skip: true }` (unquoted key, invalid JSON); if
        // the model echoes that, parse fails silently and skip never fires. This
        // test is the gate that proves Haiku produces valid JSON for the skip path.
        let config = Config::default();
        let router = HaikuRouter::from_config_with_timeout(&config, LIVE_TIMEOUT).expect("client");
        let out = router.plan("hi").expect("plan");
        assert!(
            matches!(out, RouterOutput::Skip),
            "expected Skip for trivial prompt, got {:?}",
            out
        );
    }
}
