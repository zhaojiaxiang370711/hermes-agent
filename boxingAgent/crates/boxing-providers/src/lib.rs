//! LLM providers for boxingAgent.
//!
//! Provider-agnostic chat surface: a [`Provider`] trait with a 1-shot
//! [`Provider::complete`] and a token-delta [`Provider::stream`], plus the
//! normalized request / response types. The trait is object-safe so the agent
//! loop (Phase 2) can hold a `Box<dyn Provider>` regardless of backend.
//!
//! Phase 1b ships two impls (OpenAI-compatible, Anthropic) and a config/.env
//! resolver that picks one — added by tasks S4–S6.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod openai;
pub mod resolver;

mod sse;

pub use anthropic::Anthropic;
pub use openai::OpenAiCompat;
pub use resolver::resolve;

/// A streaming chat response: an owned, `Send` stream of `StreamEvent`s.
///
/// Owned (no borrow on `&self` / `&ChatRequest`) so it can outlive the call and
/// be driven by the agent loop's own task. reqwest's response byte stream is
/// `'static + Send` once the `Response` is owned, which is how the impls build it.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("could not decode provider response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("missing required field in response: {0}")]
    Missing(&'static str),
    #[error("provider requires max_tokens to be set")]
    MaxTokensRequired,
    #[error("provider configuration error: {0}")]
    Config(String),
    #[error("stream ended before completion")]
    UnexpectedEof,
}

/// One message in a chat exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: content.into() }
    }
}

/// Provider-agnostic chat request.
///
/// `max_tokens` is optional for OpenAI-compatible providers but **required** by
/// the Anthropic API; the Anthropic impl errors if it is `None`. Each provider
/// maps this into its own wire body (see the `openai` / `anthropic` modules).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub tools: Vec<ToolDef>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self { model: model.into(), messages, max_tokens: None, stream: false, tools: vec![] }
    }
}

/// 发给模型的工具定义（provider 无关）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// Token-usage accounting, normalized across providers.
///
/// OpenAI's `prompt_tokens` / `completion_tokens` and Anthropic's
/// `input_tokens` / `output_tokens` both map to these fields. Fields are
/// optional because not every response or stream event carries a full bill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    pub fn new(input: u64, output: u64) -> Self {
        Self { input_tokens: Some(input), output_tokens: Some(output) }
    }
}

/// A 1-shot (non-streaming) completion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    pub usage: Usage,
    pub tool_calls: Vec<ToolCall>,
}

/// 从响应解析出的工具调用。arguments 为模型原样的 JSON 串（不在本层解析）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 流式事件：文本分片 或 完成的工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Text(String),
    ToolCall(ToolCall),
}

/// A chat completion backend. Object-safe (`dyn Provider`).
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// 1-shot (non-streaming) completion.
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError>;
    /// Streaming completion; yields `StreamEvent`s in order.
    async fn stream(&self, req: &ChatRequest) -> Result<ChatStream, ProviderError>;
}

/// Pass a response through on 2xx; otherwise read the body and return a
/// `Status` error carrying it. Shared by both provider impls.
pub(crate) async fn ensure_success(
    resp: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(ProviderError::Status { status: status.as_u16(), body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn chatmessage_roundtrips() {
        let m = ChatMessage::new("user", "hello");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn chatrequest_serializes_stream_flag() {
        let mut req = ChatRequest::new("gpt-test", vec![ChatMessage::new("user", "hi")]);
        req.stream = true;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"model\":\"gpt-test\""));
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn usage_optional_fields() {
        let none = Usage::default();
        assert_eq!(none.input_tokens, None);
        assert_eq!(none.output_tokens, None);
        let some = Usage::new(10, 20);
        assert_eq!(some.input_tokens, Some(10));
        assert_eq!(some.output_tokens, Some(20));
    }

    // Proves the trait is object-safe AND that async dispatch + the owned
    // stream return type work end-to-end through a `Box<dyn Provider>`.
    #[tokio::test]
    async fn dyn_provider_is_object_safe() {
        struct Dummy;
        #[async_trait::async_trait]
        impl Provider for Dummy {
            async fn complete(&self, _req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
                Ok(ChatResponse { content: "ok".into(), usage: Usage::new(1, 2), tool_calls: vec![] })
            }
            async fn stream(&self, _req: &ChatRequest) -> Result<ChatStream, ProviderError> {
                let s = futures::stream::iter(vec![
                    Ok(StreamEvent::Text("a".into())),
                    Ok(StreamEvent::Text("b".into())),
                ]);
                Ok(Box::pin(s))
            }
        }

        let p: Box<dyn Provider> = Box::new(Dummy);
        let req = ChatRequest::new("m", vec![ChatMessage::new("user", "x")]);

        let resp = p.complete(&req).await.unwrap();
        assert_eq!(resp.content, "ok");
        assert_eq!(resp.usage, Usage::new(1, 2));

        let mut s = p.stream(&req).await.unwrap();
        let mut joined = String::new();
        while let Some(ev) = s.next().await {
            if let StreamEvent::Text(t) = ev.unwrap() {
                joined.push_str(&t);
            }
        }
        assert_eq!(joined, "ab");
    }

    // Catalog faithfulness: the Phase 0 provider-kinds catalog (specs/) lists
    // both Phase-1 backends, and this crate implements both behind `Provider`
    // so the resolver (S6) can produce either.
    #[test]
    fn catalog_lists_both_kinds_and_resolver_implements_both() {
        const CATALOG: &str =
            include_str!("../../../specs/providers-phase1b.yaml");
        assert!(CATALOG.contains("openai_compatible"), "catalog must list openai_compatible");
        assert!(CATALOG.contains("anthropic"), "catalog must list anthropic");
        assert!(CATALOG.contains("streaming"), "catalog must describe streaming");

        fn is_provider(_: Box<dyn Provider>) {}
        is_provider(Box::new(crate::OpenAiCompat::new("http://localhost", "k")));
        is_provider(Box::new(crate::Anthropic::new("http://localhost", "k")));
    }
}
