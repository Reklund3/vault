use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::classify::{
    CLASSIFY_SYSTEM, Classification, Classifier, ClassifyError, ClassifyInput, build_user_prompt,
    parse_response,
};

/// A valid JSON reply is ~30 tokens. Gemini Flash does not emit chain-of-thought
/// by default, so 1024 is generous headroom.
const MAX_TOKENS: u32 = 1024;

/// Generic OpenAI-compatible chat-completions classifier — the index-time mirror
/// of `OpenAiCompatRouter`. The API key is held only in memory and only ever sent
/// in the auth header; only its env-var *name* lives in `vault.toml`. `Debug` is
/// intentionally not derived so the key can't leak through a debug print.
pub(crate) struct OpenAiCompatClassifier {
    url: String,
    model: String,
    api_key: String,
    auth: AuthHeader,
    http: Client,
}

/// Auth header style: `Bearer` (AI Studio Gemini) or `XGoogApiKey` (Vertex
/// express). Resolved from `[classifier].auth_header` at construction.
#[derive(Clone, Copy)]
enum AuthHeader {
    Bearer,
    XGoogApiKey,
}

impl AuthHeader {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "x-goog-api-key" => AuthHeader::XGoogApiKey,
            _ => AuthHeader::Bearer,
        }
    }
}

impl OpenAiCompatClassifier {
    pub(crate) fn from_config(config: &Config) -> Result<Self, ClassifyError> {
        Self::from_config_with_timeout(config, config.classifier_timeout())
    }

    pub(crate) fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, ClassifyError> {
        let env_var = config.classifier_api_key_env();
        let api_key = require_api_key(std::env::var(env_var).ok(), env_var)?;
        let model = require_model(config.classifier_model())?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ClassifyError::Transport(e.to_string()))?;
        Ok(Self {
            url: chat_completions_url(config.classifier_base_url()),
            model,
            api_key,
            auth: AuthHeader::parse(config.classifier_auth_header()),
            http,
        })
    }
}

impl Classifier for OpenAiCompatClassifier {
    fn classify(&self, input: &ClassifyInput) -> Result<Classification, ClassifyError> {
        let user = build_user_prompt(input);
        let request = ChatRequest {
            model: &self.model,
            temperature: 0.0,
            max_tokens: MAX_TOKENS,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: CLASSIFY_SYSTEM,
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
            .map_err(|e| ClassifyError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(ClassifyError::BadResponse(format!("HTTP {status}: {body}")));
        }

        let body: ChatResponse = resp
            .json()
            .map_err(|e| ClassifyError::BadResponse(e.to_string()))?;
        let message = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ClassifyError::BadResponse("empty choices".to_string()))?
            .message;

        let text = message
            .content
            .filter(|s| !s.is_empty())
            .or(message.reasoning)
            .ok_or_else(|| {
                ClassifyError::BadResponse("no content or reasoning in reply".to_string())
            })?;

        parse_response(&text)
    }
}

/// Build the chat-completions URL by string concat, NOT `Url::join` — the base
/// URL carries a path that a leading-slash join would silently drop. See the
/// router-side note in `retrieve/router/openai_compat.rs`.
fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

fn require_api_key(value: Option<String>, env_var: &str) -> Result<String, ClassifyError> {
    match value {
        Some(key) if !key.trim().is_empty() => Ok(key),
        _ => Err(ClassifyError::MissingApiKey {
            env_var: env_var.to_string(),
        }),
    }
}

/// `model` is sent verbatim; an Anthropic alias under the openai backend is a
/// misconfiguration. Reject it with guidance rather than POST a bogus model id.
fn require_model(model: &str) -> Result<String, ClassifyError> {
    match model.trim().to_ascii_lowercase().as_str() {
        "haiku" | "sonnet" | "opus" => Err(ClassifyError::Misconfigured(format!(
            "[classifier].model = {model:?} is an Anthropic alias but the openai backend is \
             selected; set it to a provider model id (e.g. \"gemini-3.5-flash\")"
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
        assert_eq!(
            chat_completions_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
    }

    #[test]
    fn require_model_rejects_anthropic_aliases() {
        assert!(matches!(
            require_model("haiku"),
            Err(ClassifyError::Misconfigured(_))
        ));
        assert_eq!(
            require_model("gemini-3.5-flash").unwrap(),
            "gemini-3.5-flash"
        );
    }

    #[test]
    fn deserializes_openai_response_and_classifies() {
        let raw = r#"{
            "choices": [
                {"message": {"role": "assistant",
                             "content": "{\"doc_type\": \"contract\", \"language\": \"proto\"}"}}
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
        let c = parse_response(&content).expect("parse");
        assert_eq!(c.doc_type, DocType::Contract);
        assert_eq!(c.language, Language::Proto);
    }
}
