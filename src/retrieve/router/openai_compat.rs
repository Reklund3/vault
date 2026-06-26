use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::retrieve::RouterOutput;
use crate::retrieve::router::{
    ROUTER_SYSTEM, Router, RouterError, build_user_prompt, parse_response,
};

// Timeout is configurable via [router].timeout (default 3s per CLAUDE.md).
/// A handful of small arrays (or the skip shortcut). 1024 is generous headroom;
/// Gemini Flash does not emit chain-of-thought by default, so we never send the
/// mlx-only `chat_template_kwargs` toggle here.
const MAX_TOKENS: u32 = 1024;

/// Generic OpenAI-compatible chat-completions router. Serves any provider that
/// speaks `/chat/completions` with a static API key — primarily Google's AI
/// Studio Gemini API (`Authorization: Bearer`) and Vertex express mode
/// (`x-goog-api-key`).
///
/// The API key is held only in memory and only ever sent in the auth header —
/// never logged, never placed in an error string, never read from any file vault
/// writes (only its env-var *name* lives in `vault.toml`). `Debug` is
/// intentionally not derived so the key can't leak through a debug print.
pub(crate) struct OpenAiCompatRouter {
    url: String,
    model: String,
    api_key: String,
    auth: AuthHeader,
    http: Client,
}

/// Auth header style. `Bearer` for AI Studio Gemini; `XGoogApiKey` for Vertex
/// express. Resolved from `[router].auth_header` at construction.
#[derive(Clone, Copy)]
enum AuthHeader {
    Bearer,
    XGoogApiKey,
}

impl AuthHeader {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "x-goog-api-key" => AuthHeader::XGoogApiKey,
            // "bearer" and anything unrecognized default to Bearer (the AI Studio
            // shape) — the most common case and a safe default.
            _ => AuthHeader::Bearer,
        }
    }
}

impl OpenAiCompatRouter {
    pub(crate) fn from_config(config: &Config) -> Result<Self, RouterError> {
        Self::from_config_with_timeout(config, config.router_timeout())
    }

    pub(crate) fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, RouterError> {
        let env_var = config.router_api_key_env();
        let api_key = require_api_key(std::env::var(env_var).ok(), env_var)?;
        let model = require_model(config.router_model())?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| RouterError::Transport(e.to_string()))?;
        Ok(Self {
            url: chat_completions_url(config.router_base_url()),
            model,
            api_key,
            auth: AuthHeader::parse(config.router_auth_header()),
            http,
        })
    }
}

impl Router for OpenAiCompatRouter {
    fn name(&self) -> &'static str {
        "openai"
    }

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

        let req = self.http.post(&self.url);
        let req = match self.auth {
            AuthHeader::Bearer => req.header("Authorization", format!("Bearer {}", self.api_key)),
            AuthHeader::XGoogApiKey => req.header("x-goog-api-key", &self.api_key),
        };

        let resp = req
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

        // Prefer the final `content`; fall back to `reasoning` for providers that
        // emit chain-of-thought separately. `parse_response` extracts the first
        // balanced JSON object from whichever field carries the payload.
        let text = message
            .content
            .filter(|s| !s.is_empty())
            .or(message.reasoning)
            .ok_or_else(|| {
                RouterError::BadResponse("no content or reasoning in reply".to_string())
            })?;

        parse_response(&text)
    }
}

/// Build the chat-completions URL by string concat, NOT `Url::join`. The base
/// URL carries a path (e.g. `.../v1beta/openai`); `Url::join("/chat/completions")`
/// would treat the leading slash as absolute and silently drop that path.
fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

fn require_api_key(value: Option<String>, env_var: &str) -> Result<String, RouterError> {
    match value {
        Some(key) if !key.trim().is_empty() => Ok(key),
        _ => Err(RouterError::MissingApiKey {
            env_var: env_var.to_string(),
        }),
    }
}

/// The openai backend sends `model` verbatim. An Anthropic alias left over from a
/// haiku config is a misconfiguration — reject it with guidance rather than POST
/// a bogus model id and get an opaque provider 4xx.
fn require_model(model: &str) -> Result<String, RouterError> {
    match model.trim().to_ascii_lowercase().as_str() {
        "haiku" | "sonnet" | "opus" => Err(RouterError::Misconfigured(format!(
            "[router].model = {model:?} is an Anthropic alias but the openai backend is selected; \
             set it to a provider model id (e.g. \"gemini-3.5-flash\")"
        ))),
        _ => Ok(model.to_string()),
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
    #[serde(default)]
    reasoning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DocType, Language};

    #[test]
    fn chat_completions_url_preserves_base_path() {
        // Regression guard for the Url::join path-drop trap: the AI Studio base
        // carries `/v1beta/openai`, which must survive into the final URL.
        assert_eq!(
            chat_completions_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
        // Trailing slash on the base must not double up.
        assert_eq!(
            chat_completions_url("https://aiplatform.googleapis.com/v1/"),
            "https://aiplatform.googleapis.com/v1/chat/completions"
        );
    }

    #[test]
    fn require_api_key_rejects_missing_and_blank() {
        assert!(matches!(
            require_api_key(None, "GEMINI_API_KEY"),
            Err(RouterError::MissingApiKey { .. })
        ));
        assert!(matches!(
            require_api_key(Some("   ".to_string()), "GEMINI_API_KEY"),
            Err(RouterError::MissingApiKey { .. })
        ));
        assert_eq!(
            require_api_key(Some("k".to_string()), "GEMINI_API_KEY").unwrap(),
            "k"
        );
    }

    #[test]
    fn require_model_rejects_anthropic_aliases() {
        for alias in ["haiku", "Sonnet", "OPUS"] {
            assert!(
                matches!(require_model(alias), Err(RouterError::Misconfigured(_))),
                "expected Misconfigured for {alias}"
            );
        }
        assert_eq!(require_model("gemini-3.5-flash").unwrap(), "gemini-3.5-flash");
    }

    #[test]
    fn deserializes_openai_plan_response() {
        // Same chat-completions shape as Gemma, but with no chat_template_kwargs
        // on the wire — confirms the generic client parses a provider reply.
        let raw = r#"{
            "choices": [
                {"message": {"role": "assistant",
                             "content": "{\"projects\":[\"vault\"],\"doc_types\":[\"contract\"],\"languages\":[\"proto\"]}"}}
            ]
        }"#;
        let body: ChatResponse = serde_json::from_str(raw).expect("deserialize");
        let content = body
            .choices
            .into_iter()
            .next()
            .unwrap()
            .message
            .content
            .expect("content");
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
}
