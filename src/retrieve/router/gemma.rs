use std::time::Duration;

use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::retrieve::RouterOutput;
use crate::retrieve::router::{
    ROUTER_SYSTEM, Router, RouterError, build_user_prompt, parse_response,
};

// Timeout is configurable via [router].timeout_secs (default 3s per CLAUDE.md).
// The hook caller silences failures (passthrough), so the router's only job is
// to fail inside whatever budget the user configured.
/// Five small arrays + the skip shortcut would fit in ~256 tokens, but Gemma 4
/// (and other thinking models) burn hundreds of tokens on internal reasoning
/// before emitting the answer. 1024 gives room to think and still leave space
/// for the JSON. The 3s hard cap remains the binding constraint on this path.
const MAX_TOKENS: u32 = 1024;

pub(crate) struct GemmaRouter {
    endpoint: Url,
    model: String,
    http: Client,
}

impl GemmaRouter {
    pub(crate) fn from_config(config: &Config) -> Result<Self, RouterError> {
        Self::from_config_with_timeout(config, config.router_timeout())
    }

    pub(crate) fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, RouterError> {
        let endpoint = Url::parse(config.mlx_endpoint())
            .map_err(|e| RouterError::Transport(format!("bad mlx endpoint: {e}")))?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| RouterError::Transport(e.to_string()))?;
        Ok(Self {
            endpoint,
            model: config.mlx_model().to_string(),
            http,
        })
    }
}

impl Router for GemmaRouter {
    fn plan(&self, prompt: &str) -> Result<RouterOutput, RouterError> {
        let user = build_user_prompt(prompt);
        let request = ChatRequest {
            model: &self.model,
            temperature: 0.0,
            max_tokens: MAX_TOKENS,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: ROUTER_SYSTEM,
                },
                ChatMessage {
                    role: "user",
                    content: &user,
                },
            ],
        };

        let url = self
            .endpoint
            .join("/v1/chat/completions")
            .map_err(|e| RouterError::Transport(e.to_string()))?;

        let resp = self
            .http
            .post(url)
            .json(&request)
            .send()
            .map_err(|e| RouterError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(RouterError::BadResponse(format!("HTTP {status}: {body}")));
        }

        let body: ChatResponse = resp
            .json()
            .map_err(|e| RouterError::BadResponse(e.to_string()))?;
        let message = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| RouterError::BadResponse("empty choices".to_string()))?
            .message;

        // Prefer the final `content` channel; fall back to `reasoning` when the
        // model emits everything as thinking (e.g. Gemma 4 with the thinking
        // template auto-enabled). `parse_response` extracts the first balanced
        // JSON object from whichever field carries the payload.
        let text = message
            .content
            .filter(|s| !s.is_empty())
            .or(message.reasoning)
            .ok_or_else(|| RouterError::BadResponse("no content or reasoning in reply".to_string()))?;

        parse_response(&text)
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    /// Some mlx_lm.server / Gemma 4 builds emit chain-of-thought output as a
    /// separate `reasoning` field instead of (or alongside) `content`. We treat
    /// it as a fallback payload — the JSON extractor handles either source.
    #[serde(default)]
    reasoning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DocType, Language};

    #[test]
    fn deserializes_openai_plan_response() {
        let raw = r#"{
            "choices": [
                {"message": {"role": "assistant",
                             "content": "{\"projects\":[\"vault\"],\"type_names\":[\"BuildRequest\"],\"topics\":[],\"doc_types\":[\"contract\"],\"languages\":[\"proto\"]}"}}
            ]
        }"#;
        let body: ChatResponse = serde_json::from_str(raw).expect("deserialize");
        let content = body.choices.into_iter().next().unwrap().message.content.expect("content");
        let out = parse_response(&content).expect("parse");
        match out {
            RouterOutput::Plan(plan) => {
                assert_eq!(plan.projects, vec!["vault"]);
                assert_eq!(plan.doc_types, vec![DocType::Contract]);
                assert_eq!(plan.languages, vec![Language::Proto]);
            }
            RouterOutput::Skip => panic!("expected Plan"),
        }
    }

    #[test]
    fn deserializes_openai_skip_response() {
        let raw = r#"{
            "choices": [
                {"message": {"role": "assistant", "content": "{ \"skip\": true }"}}
            ]
        }"#;
        let body: ChatResponse = serde_json::from_str(raw).expect("deserialize");
        let content = body.choices.into_iter().next().unwrap().message.content.expect("content");
        let out = parse_response(&content).expect("parse");
        assert!(matches!(out, RouterOutput::Skip));
    }

    #[test]
    fn deserializes_reasoning_only_response() {
        // Some Gemma 4 builds (or any thinking model behind mlx) emit the JSON
        // inside a `reasoning` field with no `content`. The router extracts it
        // via the reasoning fallback.
        let raw = r#"{
            "choices": [
                {"message": {"role": "assistant",
                             "reasoning": "User wants no context… { \"skip\": true }"}}
            ]
        }"#;
        let body: ChatResponse = serde_json::from_str(raw).expect("deserialize");
        let msg = body.choices.into_iter().next().unwrap().message;
        let text = msg.content.filter(|s| !s.is_empty()).or(msg.reasoning).unwrap();
        let out = parse_response(&text).expect("parse");
        assert!(matches!(out, RouterOutput::Skip));
    }

    /// 30s for live tests: production sets the budget via
    /// `[router].timeout_secs` in `vault.toml`. The live test fixes a generous
    /// number explicitly so it doesn't depend on the user's config file.
    const LIVE_TIMEOUT: Duration = Duration::from_secs(180);

    #[test]
    #[ignore = "requires live mlx_lm.server at http://localhost:8080"]
    fn live_gemma_route_plan() {
        let config = Config::default();
        let router = GemmaRouter::from_config_with_timeout(&config, LIVE_TIMEOUT).expect("client");
        let out = router.plan("How does the BuildRequest proto handle retries?").expect("plan");
        // Don't assert exact shape — model output drifts. Just confirm it parses.
        let _ = out;
    }

    #[test]
    #[ignore = "requires live mlx_lm.server at http://localhost:8080"]
    fn live_gemma_route_skip() {
        // Skip is the zero-cost-passthrough optimization for the hook. ROUTER_SYSTEM
        // shows the skip shape as `{ skip: true }` (unquoted key, invalid JSON); if
        // the model echoes that, parse fails silently and skip never fires. This
        // test is the gate that proves Gemma produces valid JSON for the skip path.
        let config = Config::default();
        let router = GemmaRouter::from_config_with_timeout(&config, LIVE_TIMEOUT).expect("client");
        let out = router.plan("hi").expect("plan");
        assert!(matches!(out, RouterOutput::Skip), "expected Skip for trivial prompt, got {:?}", out);
    }
}
