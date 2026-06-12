use std::time::Duration;

use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::index::classify::{
    CLASSIFY_SYSTEM, Classification, Classifier, ClassifyError, ClassifyInput, build_user_prompt,
    parse_response,
};

/// A valid JSON reply is ~30 tokens, but Gemma 4 (and other thinking models)
/// burn hundreds of tokens on internal reasoning before emitting the answer.
/// 1024 gives the model room to think and still leave space for the JSON.
const MAX_TOKENS: u32 = 1024;

pub(crate) struct GemmaClassifier {
    endpoint: Url,
    model: String,
    http: Client,
}

impl GemmaClassifier {
    pub(crate) fn from_config(config: &Config) -> Result<Self, ClassifyError> {
        Self::from_config_with_timeout(config, config.classifier_timeout())
    }

    pub(crate) fn from_config_with_timeout(
        config: &Config,
        timeout: Duration,
    ) -> Result<Self, ClassifyError> {
        let endpoint = Url::parse(config.mlx_endpoint())
            .map_err(|e| ClassifyError::Transport(format!("bad mlx endpoint: {e}")))?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ClassifyError::Transport(e.to_string()))?;
        Ok(Self {
            endpoint,
            model: config.mlx_model().to_string(),
            http,
        })
    }
}

impl Classifier for GemmaClassifier {
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

        let url = self
            .endpoint
            .join("/v1/chat/completions")
            .map_err(|e| ClassifyError::Transport(e.to_string()))?;

        let resp = self
            .http
            .post(url)
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

        // Prefer the final `content` channel; fall back to `reasoning` when the
        // model emits everything as thinking (e.g. Gemma 4 with the thinking
        // template auto-enabled). `parse_response` extracts the first balanced
        // JSON object from whichever field carries the payload.
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
    fn deserializes_openai_response_and_classifies() {
        // Shape mlx_lm.server returns: OpenAI chat-completions with the JSON
        // classification as the assistant message content.
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

    #[test]
    fn deserializes_reasoning_only_response() {
        // Some Gemma 4 builds (or any thinking model behind mlx) emit the JSON
        // inside a `reasoning` field with no `content`. The classifier extracts
        // it via the reasoning fallback.
        let raw = r#"{
            "choices": [
                {"message": {"role": "assistant",
                             "reasoning": "Looking at the proto syntax… {\"doc_type\": \"contract\", \"language\": \"proto\"}"}}
            ]
        }"#;
        let body: ChatResponse = serde_json::from_str(raw).expect("deserialize");
        let msg = body.choices.into_iter().next().unwrap().message;
        let text = msg
            .content
            .filter(|s| !s.is_empty())
            .or(msg.reasoning)
            .unwrap();
        let c = parse_response(&text).expect("parse");
        assert_eq!(c.doc_type, DocType::Contract);
    }

    #[test]
    #[ignore = "requires live mlx_lm.server at http://localhost:8080"]
    fn live_gemma_classify() {
        let config = Config::default();
        let classifier = GemmaClassifier::from_config(&config).expect("client");
        let input = ClassifyInput {
            filename: "build.proto".to_string(),
            extension: "proto".to_string(),
            head: "syntax = \"proto3\";\nmessage BuildRequest { string id = 1; }".to_string(),
        };
        let c = classifier.classify(&input).expect("classify");
        assert_eq!(c.doc_type, DocType::Contract);
    }
}
