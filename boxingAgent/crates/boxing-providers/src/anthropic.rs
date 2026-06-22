//! Anthropic Messages API provider.
//!
//! Hits `{base_url}/v1/messages` with `x-api-key` + `anthropic-version`
//! headers. Non-stream returns `content[0].text` + `usage` (input/output
//! tokens); streaming is SSE with typed events — token deltas arrive as
//! `content_block_delta` (`delta.text`), the stream ends on `message_stop`.
//! `max_tokens` is required by the API and enforced up front.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::sse::SseLineStream;
use crate::{
    ensure_success, ChatMessage, ChatRequest, ChatResponse, ChatStream, Provider, ProviderError,
    TokenDelta, Usage,
};

#[derive(Clone)]
pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Anthropic {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    /// Build with a custom reqwest client (custom TLS, timeouts, etc.).
    pub fn with_client(
        client: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self { client, base_url: base_url.into(), api_key: api_key.into() }
    }

    fn url(&self, suffix: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}{suffix}")
    }
}

#[async_trait::async_trait]
impl Provider for Anthropic {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        let max_tokens = req.max_tokens.ok_or(ProviderError::MaxTokensRequired)?;
        let body = AnthropicBody {
            model: &req.model,
            max_tokens,
            messages: &req.messages,
            stream: false,
        };
        let resp = self
            .client
            .post(self.url("/v1/messages"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        let resp = ensure_success(resp).await?;
        let parsed: AnthropicResponse = resp.json().await?;
        let content = parsed
            .content
            .first()
            .and_then(|b| b.text.clone())
            .ok_or(ProviderError::Missing("content[0].text"))?;
        let usage = parsed
            .usage
            .map(|u| Usage { input_tokens: u.input_tokens, output_tokens: u.output_tokens })
            .unwrap_or_default();
        Ok(ChatResponse { content, usage })
    }

    async fn stream(&self, req: &ChatRequest) -> Result<ChatStream, ProviderError> {
        let max_tokens = req.max_tokens.ok_or(ProviderError::MaxTokensRequired)?;
        let body = AnthropicBody {
            model: &req.model,
            max_tokens,
            messages: &req.messages,
            stream: true,
        };
        let resp = self
            .client
            .post(self.url("/v1/messages"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        let resp = ensure_success(resp).await?;
        let bytes = resp.bytes_stream();
        let deltas = AnthropicDeltaStream { lines: SseLineStream::new(bytes) };
        Ok(Box::pin(deltas))
    }
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Wire body for `/v1/messages`.
#[derive(Serialize)]
struct AnthropicBody<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[derive(Deserialize)]
struct AnthropicBlock {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

/// One SSE event from the streaming Messages API (only the fields we act on).
#[derive(Deserialize)]
struct AnthropicEvent {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    delta: Option<AnthropicDelta>,
}

#[derive(Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// Parses the SSE line stream into token deltas: emits the `text` of each
/// `content_block_delta` (text_delta) and terminates on `message_stop`.
struct AnthropicDeltaStream<S: Unpin> {
    lines: SseLineStream<S>,
}

impl<S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin> Stream for AnthropicDeltaStream<S> {
    type Item = Result<TokenDelta, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.lines).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(payload))) => {
                    if payload == "[DONE]" {
                        return Poll::Ready(None);
                    }
                    let event: AnthropicEvent = match serde_json::from_str(&payload) {
                        Ok(e) => e,
                        Err(e) => return Poll::Ready(Some(Err(ProviderError::Decode(e)))),
                    };
                    match event.kind.as_deref() {
                        Some("message_stop") => return Poll::Ready(None),
                        Some("content_block_delta") => {
                            let text = event.delta.as_ref().and_then(|d| {
                                if d.kind.as_deref() == Some("text_delta") {
                                    d.text.clone()
                                } else {
                                    None
                                }
                            });
                            match text {
                                Some(t) if !t.is_empty() => {
                                    return Poll::Ready(Some(Ok(TokenDelta::new(t))))
                                }
                                _ => continue,
                            }
                        }
                        _ => continue,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn req() -> ChatRequest {
        let mut r = ChatRequest::new("claude-test", vec![ChatMessage::new("user", "hi")]);
        r.max_tokens = Some(32);
        r
    }

    #[tokio::test]
    async fn complete_returns_content_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "hello there"}],
                "usage": {"input_tokens": 8, "output_tokens": 2}
            })))
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "k");
        let resp = p.complete(&req()).await.unwrap();
        assert_eq!(resp.content, "hello there");
        assert_eq!(resp.usage.input_tokens, Some(8));
        assert_eq!(resp.usage.output_tokens, Some(2));
    }

    #[tokio::test]
    async fn complete_requires_max_tokens() {
        // max_tokens is mandatory for the Messages API; enforced before any HTTP.
        let mut req = ChatRequest::new("claude-test", vec![ChatMessage::new("user", "hi")]);
        req.max_tokens = None;
        let p = Anthropic::new("http://0.0.0.0:0", "k");
        match p.complete(&req).await {
            Err(ProviderError::MaxTokensRequired) => {}
            other => panic!("expected MaxTokensRequired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_requires_max_tokens() {
        let mut req = ChatRequest::new("claude-test", vec![ChatMessage::new("user", "hi")]);
        req.max_tokens = None;
        let p = Anthropic::new("http://0.0.0.0:0", "k");
        // ChatStream isn't Debug (dyn Stream), so pattern-match instead of unwrap_err.
        match p.stream(&req).await {
            Err(ProviderError::MaxTokensRequired) => {}
            Err(e) => panic!("expected MaxTokensRequired, got {e:?}"),
            Ok(_) => panic!("expected MaxTokensRequired, got a stream"),
        }
    }

    #[tokio::test]
    async fn stream_yields_deltas_in_order() {
        let server = MockServer::start().await;
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":8}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .to_string();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(body_string_contains("\"stream\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "k");
        let mut s = p.stream(&req()).await.unwrap();
        let mut out = String::new();
        while let Some(d) = s.next().await {
            out.push_str(&d.unwrap().content);
        }
        assert_eq!(out, "Hello");
    }
}
