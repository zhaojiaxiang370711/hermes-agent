//! OpenAI-compatible chat completions provider.
//!
//! Hits `{base_url}/chat/completions` — the shape used by OpenAI and any
//! OpenAI-compatible gateway (e.g. a `.../v1` endpoint). Non-stream returns
//! `choices[0].message.content` + `usage`; streaming is SSE with `data: {json}`
//! lines terminated by `data: [DONE]`, token deltas in
//! `choices[0].delta.content`.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::sse::SseLineStream;
use crate::{
    ensure_success, ChatMessage, ChatRequest, ChatResponse, ChatStream, Provider, ProviderError,
    StreamEvent, Usage,
};

#[derive(Clone)]
pub struct OpenAiCompat {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OpenAiCompat {
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
impl Provider for OpenAiCompat {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        let body = OpenAiBody {
            model: &req.model,
            messages: &req.messages,
            max_tokens: req.max_tokens,
            stream: false,
        };
        let resp = self
            .client
            .post(self.url("/chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let resp = ensure_success(resp).await?;
        let parsed: OpenAiResponse = resp.json().await?;
        let content = parsed
            .choices
            .first()
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.content.clone())
            .ok_or(ProviderError::Missing("choices[0].message.content"))?;
        let usage = parsed
            .usage
            .map(|u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            })
            .unwrap_or_default();
        Ok(ChatResponse { content, usage })
    }

    async fn stream(&self, req: &ChatRequest) -> Result<ChatStream, ProviderError> {
        let body = OpenAiBody {
            model: &req.model,
            messages: &req.messages,
            max_tokens: req.max_tokens,
            stream: true,
        };
        let resp = self
            .client
            .post(self.url("/chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let resp = ensure_success(resp).await?;
        let bytes = resp.bytes_stream();
        let deltas = OpenAiDeltaStream { lines: SseLineStream::new(bytes) };
        Ok(Box::pin(deltas))
    }
}

/// Wire body for `/chat/completions`.
#[derive(Serialize)]
struct OpenAiBody<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
}

#[derive(Deserialize)]
struct OpenAiContent {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    message: Option<OpenAiContent>,
    #[serde(default)]
    delta: Option<OpenAiContent>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct OpenAiResponse {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

/// Parses the SSE line stream into token deltas: emits `choices[0].delta.content`
/// (skipping empty/role chunks) and terminates on `data: [DONE]`.
struct OpenAiDeltaStream<S: Unpin> {
    lines: SseLineStream<S>,
}

impl<S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin> Stream for OpenAiDeltaStream<S> {
    type Item = Result<StreamEvent, ProviderError>;

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
                    let chunk: OpenAiResponse = match serde_json::from_str(&payload) {
                        Ok(c) => c,
                        Err(e) => return Poll::Ready(Some(Err(ProviderError::Decode(e)))),
                    };
                    let text = chunk
                        .choices
                        .first()
                        .and_then(|c| c.delta.as_ref())
                        .and_then(|d| d.content.clone());
                    match text {
                        Some(t) if !t.is_empty() => return Poll::Ready(Some(Ok(StreamEvent::Text(t)))),
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
        let mut r = ChatRequest::new("gpt-test", vec![ChatMessage::new("user", "hi")]);
        r.max_tokens = Some(16);
        r
    }

    #[tokio::test]
    async fn complete_returns_content_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hello there"}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 3}
            })))
            .mount(&server)
            .await;

        let p = OpenAiCompat::new(server.uri(), "k");
        let resp = p.complete(&req()).await.unwrap();
        assert_eq!(resp.content, "hello there");
        assert_eq!(resp.usage.input_tokens, Some(5));
        assert_eq!(resp.usage.output_tokens, Some(3));
    }

    #[tokio::test]
    async fn complete_errors_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let p = OpenAiCompat::new(server.uri(), "k");
        match p.complete(&req()).await.unwrap_err() {
            ProviderError::Status { status, body } => {
                assert_eq!(status, 401);
                assert_eq!(body, "unauthorized");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_yields_deltas_in_order() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("\"stream\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
            .mount(&server)
            .await;

        let p = OpenAiCompat::new(server.uri(), "k");
        let mut s = p.stream(&req()).await.unwrap();
        let mut out = String::new();
        while let Some(ev) = s.next().await {
            if let StreamEvent::Text(t) = ev.unwrap() {
                out.push_str(&t);
            }
        }
        assert_eq!(out, "Hello");
    }
}
