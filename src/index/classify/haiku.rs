use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::classify::{
    CLASSIFY_SYSTEM, Classification, Classifier, ClassifyError, ClassifyInput, build_user_prompt,
    parse_response,
};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TOKENS: u32 = 128;

/// Anthropic Haiku classifier. The API key is held only in memory and only ever
/// sent in the `x-api-key` header — never logged, never placed in an error
/// string, never read from any file vault writes. `Debug` is intentionally not
/// derived so the key can't leak through a debug print.
pub(crate) struct HaikuClassifier {
    model: String,
    api_key: String,
    http: Client,
}

impl HaikuClassifier {
    pub(crate) fn from_config(config: &Config) -> Result<Self, ClassifyError> {
        Self::from_config_with_timeout(config, DEFAULT_TIMEOUT)
    }

    pub(crate) fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, ClassifyError> {
        let api_key = require_api_key(std::env::var("ANTHROPIC_API_KEY").ok())?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ClassifyError::Transport(e.to_string()))?;
        Ok(Self {
            model: resolve_model(config.classifier_model()),
            api_key,
            http,
        })
    }
}

impl Classifier for HaikuClassifier {
    fn classify(&self, input: &ClassifyInput) -> Result<Classification, ClassifyError> {
        let user = build_user_prompt(input);
        let request = MessagesRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            // The system block carries the byte-identical CLASSIFY_SYSTEM behind
            // an ephemeral cache so only the per-file user turn is fresh input.
            system: vec![SystemBlock {
                kind: "text",
                text: CLASSIFY_SYSTEM,
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
            .map_err(|e| ClassifyError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            // Anthropic error bodies never echo the request key.
            let body = resp.text().unwrap_or_default();
            return Err(ClassifyError::BadResponse(format!("HTTP {status}: {body}")));
        }

        let body: MessagesResponse = resp
            .json()
            .map_err(|e| ClassifyError::BadResponse(e.to_string()))?;
        let text = body
            .content
            .into_iter()
            .next()
            .ok_or_else(|| ClassifyError::BadResponse("empty content".to_string()))?
            .text;

        parse_response(&text)
    }
}

fn require_api_key(value: Option<String>) -> Result<String, ClassifyError> {
    match value {
        Some(key) if !key.trim().is_empty() => Ok(key),
        _ => Err(ClassifyError::MissingApiKey),
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

/// Order-of-magnitude classification cost in USD for sync's one-time `[y/N]`
/// confirmation prompt. Uses the plan's rough cached per-call rate; the
/// first-call cache-write is folded in as rounding margin. Not a billing figure.
pub(crate) fn cost_estimate(file_count: usize) -> f64 {
    const PER_FILE_USD: f64 = 0.0002;
    file_count as f64 * PER_FILE_USD
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
            Err(ClassifyError::MissingApiKey)
        ));
        assert!(matches!(
            require_api_key(Some(String::new())),
            Err(ClassifyError::MissingApiKey)
        ));
        assert!(matches!(
            require_api_key(Some("   ".to_string())),
            Err(ClassifyError::MissingApiKey)
        ));
        assert_eq!(
            require_api_key(Some("sk-test".to_string())).unwrap(),
            "sk-test"
        );
    }

    #[test]
    fn deserializes_messages_response_and_classifies() {
        let raw = r#"{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "{\"doc_type\": \"convention\", \"language\": \"go\"}"}],
            "stop_reason": "end_turn"
        }"#;
        let body: MessagesResponse = serde_json::from_str(raw).expect("deserialize");
        let text = body.content.into_iter().next().unwrap().text;
        let c = parse_response(&text).expect("parse");
        assert_eq!(c.doc_type, DocType::Convention);
        assert_eq!(c.language, Language::Go);
    }

    #[test]
    fn empty_content_is_bad_response() {
        let raw = r#"{"id":"m","type":"message","role":"assistant","content":[]}"#;
        let body: MessagesResponse = serde_json::from_str(raw).unwrap();
        let result = body
            .content
            .into_iter()
            .next()
            .ok_or_else(|| ClassifyError::BadResponse("empty content".to_string()))
            .map(|b| b.text);
        assert!(matches!(result, Err(ClassifyError::BadResponse(_))));
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
            Err(ClassifyError::BadResponse(_))
        ));
    }

    #[test]
    fn cost_estimate_is_reasonable() {
        assert_eq!(cost_estimate(0), 0.0);
        let two_hundred = cost_estimate(200);
        assert!(two_hundred > 0.0 && two_hundred < 1.0, "got {two_hundred}");
    }

    #[test]
    #[ignore = "requires ANTHROPIC_API_KEY and network"]
    fn live_haiku_classify() {
        let config = Config::default();
        let classifier = HaikuClassifier::from_config(&config).expect("client");
        let input = ClassifyInput {
            filename: "build.proto".to_string(),
            extension: "proto".to_string(),
            head: "syntax = \"proto3\";\nmessage BuildRequest { string id = 1; }".to_string(),
        };
        let c = classifier.classify(&input).expect("classify");
        assert_eq!(c.doc_type, DocType::Contract);
    }
}
