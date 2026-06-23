//! Anthropic Messages API provider.
//!
//! Hits `{base_url}/v1/messages` with `x-api-key` + `anthropic-version` headers.
//! Non-stream: `content[]` 文本块 + `tool_use` 块 + `usage`；流式：SSE 事件——
//! 文本走 `content_block_delta`(`text_delta`)，工具调用经
//! `content_block_start`(`tool_use`) + `content_block_delta`(`input_json_delta`)
//! 累积，`content_block_stop` 时发 `StreamEvent::ToolCall`，`message_stop` 结束。
//! `max_tokens` 必填。

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::sse::SseLineStream;
use crate::{
    ensure_success, ChatMessage, ChatRequest, ChatResponse, ChatStream, Provider, ProviderError,
    StreamEvent, ToolCall, Usage,
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
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
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
        let tools = anthropic_tools(req);
        let (system, messages) = to_anthropic_messages(&req.messages);
        let body = AnthropicBody {
            model: &req.model,
            max_tokens,
            messages,
            system,
            stream: false,
            tools,
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
            .unwrap_or_default();
        let tool_calls = parsed
            .content
            .iter()
            .filter(|b| b.kind.as_deref() == Some("tool_use"))
            .filter_map(|b| {
                let id = b.id.clone()?;
                let name = b.name.clone()?;
                let arguments = b
                    .input
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                Some(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect();
        let usage = parsed
            .usage
            .map(|u| Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            })
            .unwrap_or_default();
        Ok(ChatResponse {
            content,
            usage,
            tool_calls,
        })
    }

    async fn stream(&self, req: &ChatRequest) -> Result<ChatStream, ProviderError> {
        let max_tokens = req.max_tokens.ok_or(ProviderError::MaxTokensRequired)?;
        let tools = anthropic_tools(req);
        let (system, messages) = to_anthropic_messages(&req.messages);
        let body = AnthropicBody {
            model: &req.model,
            max_tokens,
            messages,
            system,
            stream: true,
            tools,
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
        let deltas = AnthropicDeltaStream {
            lines: SseLineStream::new(bytes),
            blocks: HashMap::new(),
            pending: VecDeque::new(),
        };
        Ok(Box::pin(deltas))
    }
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 从 `ChatRequest.tools` 构建 Anthropic tools（input_schema 包装）。
fn anthropic_tools(req: &ChatRequest) -> Vec<AnthropicTool<'_>> {
    req.tools
        .iter()
        .map(|t| AnthropicTool {
            name: t.name.as_str(),
            description: t.description.as_str(),
            input_schema: &t.parameters,
        })
        .collect()
}

/// Wire body for `/v1/messages`.
#[derive(Serialize)]
struct AnthropicBody<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

/// Anthropic 消息内容：纯文本字符串 或 content-blocks 数组。
#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Str(String),
    Blocks(Vec<serde_json::Value>),
}

/// 转 Anthropic 线消息：system 抽到顶层；assistant 工具调用 → content blocks；
/// 连续 tool 消息合并成一条 user/tool_result。
fn to_anthropic_messages(msgs: &[ChatMessage]) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system: Option<String> = None;
    let mut out: Vec<AnthropicMessage> = Vec::new();
    let mut pending: Vec<serde_json::Value> = Vec::new();
    for m in msgs {
        if m.role != "tool" {
            flush_results(&mut out, &mut pending);
        }
        match m.role.as_str() {
            "system" => {
                if system.is_none() {
                    system = Some(m.content.clone());
                }
            }
            "user" => {
                if m.images.is_empty() {
                    out.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Str(m.content.clone()),
                    });
                } else {
                    let mut blocks = vec![serde_json::json!({"type": "text", "text": m.content})];
                    for img in &m.images {
                        // 解析 data URL: data:image/jpeg;base64,xxxxx
                        let (media_type, data) = parse_data_url(img);
                        blocks.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data,
                            }
                        }));
                    }
                    out.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                }
            }
            "assistant" => {
                if let Some(tcs) = &m.tool_calls {
                    let mut blocks = Vec::new();
                    if !m.content.is_empty() {
                        blocks.push(serde_json::json!({"type":"text","text":m.content}));
                    }
                    for tc in tcs {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.arguments).unwrap_or_default();
                        blocks.push(serde_json::json!({
                            "type":"tool_use","id":tc.id,"name":tc.name,"input":input
                        }));
                    }
                    out.push(AnthropicMessage {
                        role: "assistant".into(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                } else {
                    out.push(AnthropicMessage {
                        role: "assistant".into(),
                        content: AnthropicContent::Str(m.content.clone()),
                    });
                }
            }
            "tool" => {
                pending.push(serde_json::json!({
                    "type":"tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                }));
            }
            _ => {}
        }
    }
    flush_results(&mut out, &mut pending);
    (system, out)
}

/// 把累积的 tool_result 冲刷成一条 user 消息。
/// 解析 data URL，返回 (media_type, base64_data)。
fn parse_data_url(data_url: &str) -> (&str, &str) {
    if let Some(rest) = data_url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(';') {
            if let Some(b64) = data.strip_prefix("base64,") {
                return (meta, b64);
            }
        }
    }
    ("image/jpeg", data_url) // fallback
}

fn flush_results(out: &mut Vec<AnthropicMessage>, pending: &mut Vec<serde_json::Value>) {
    if !pending.is_empty() {
        let blocks = std::mem::take(pending);
        out.push(AnthropicMessage {
            role: "user".into(),
            content: AnthropicContent::Blocks(blocks),
        });
    }
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct AnthropicBlock {
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
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

/// `content_block_start` 中的 content_block（只取我们关心的字段）。
#[derive(Deserialize)]
struct AnthropicBlockStart {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// One SSE event (only the fields we act on).
#[derive(Deserialize)]
struct AnthropicEvent {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    content_block: Option<AnthropicBlockStart>,
    #[serde(default)]
    delta: Option<AnthropicDelta>,
}

#[derive(Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

/// 按 index 累积的 tool_use 块。
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

/// 解析 SSE 行流：`text_delta` → `StreamEvent::Text`；`tool_use` 块经
/// `content_block_start` + `input_json_delta` 累积，`content_block_stop` 时
/// 发 `StreamEvent::ToolCall`；`message_stop` 结束。
struct AnthropicDeltaStream<S: Unpin> {
    lines: SseLineStream<S>,
    blocks: HashMap<u64, ToolCallAccum>,
    pending: VecDeque<ToolCall>,
}

impl<S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin> Stream for AnthropicDeltaStream<S> {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = &mut *self;
        loop {
            if let Some(tc) = me.pending.pop_front() {
                return Poll::Ready(Some(Ok(StreamEvent::ToolCall(tc))));
            }
            match Pin::new(&mut me.lines).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    if me.pending.is_empty() {
                        return Poll::Ready(None);
                    }
                    continue;
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(payload))) => {
                    if payload == "[DONE]" {
                        continue; // Anthropic 用 message_stop，兼容 [DONE]
                    }
                    let event: AnthropicEvent = match serde_json::from_str(&payload) {
                        Ok(e) => e,
                        Err(e) => return Poll::Ready(Some(Err(ProviderError::Decode(e)))),
                    };
                    match event.kind.as_deref() {
                        Some("message_stop") => {
                            if me.pending.is_empty() {
                                return Poll::Ready(None);
                            }
                            continue;
                        }
                        Some("content_block_start") => {
                            if let (Some(idx), Some(cb)) =
                                (event.index, event.content_block.as_ref())
                            {
                                if cb.kind.as_deref() == Some("tool_use") {
                                    me.blocks.insert(
                                        idx,
                                        ToolCallAccum {
                                            id: cb.id.clone().unwrap_or_default(),
                                            name: cb.name.clone().unwrap_or_default(),
                                            arguments: String::new(),
                                        },
                                    );
                                }
                            }
                            continue;
                        }
                        Some("content_block_delta") => {
                            if let Some(d) = event.delta.as_ref() {
                                match d.kind.as_deref() {
                                    Some("text_delta") => {
                                        // 文本无需 index（单一文本流）
                                        if let Some(t) = d.text.as_ref() {
                                            if !t.is_empty() {
                                                return Poll::Ready(Some(Ok(StreamEvent::Text(
                                                    t.clone(),
                                                ))));
                                            }
                                        }
                                    }
                                    Some("input_json_delta") => {
                                        // 工具入参按 index 累积到对应块
                                        if let (Some(idx), Some(p)) =
                                            (event.index, d.partial_json.as_ref())
                                        {
                                            if let Some(b) = me.blocks.get_mut(&idx) {
                                                b.arguments.push_str(p);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            continue;
                        }
                        Some("content_block_stop") => {
                            if let Some(idx) = event.index {
                                if let Some(a) = me.blocks.remove(&idx) {
                                    me.pending.push_back(ToolCall {
                                        id: a.id,
                                        name: a.name,
                                        arguments: a.arguments,
                                    });
                                }
                            }
                            continue;
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
    use crate::ToolDef;
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
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn complete_parses_tool_use_and_sends_tools() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(body_string_contains("\"input_schema\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "tool_use", "id": "tu_1", "name": "read", "input": {"path": "a.rs"}}],
                "usage": {"input_tokens": 8, "output_tokens": 2}
            })))
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "k");
        let mut r = req();
        r.tools.push(ToolDef {
            name: "read".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type": "object"}),
        });
        let resp = p.complete(&r).await.unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tu_1");
        assert_eq!(resp.tool_calls[0].name, "read");
        assert_eq!(resp.tool_calls[0].arguments, "{\"path\":\"a.rs\"}");
    }

    #[tokio::test]
    async fn complete_sends_tool_history_and_system() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .and(body_string_contains("\"system\":\"SYS\""))
            .and(body_string_contains("\"tool_use\""))
            .and(body_string_contains("\"tool_result\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "done"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&server)
            .await;
        let p = Anthropic::new(server.uri(), "k");
        let mut r = ChatRequest::new("claude-test", vec![]);
        r.max_tokens = Some(32);
        r.messages.push(ChatMessage::new("system", "SYS"));
        r.messages.push(ChatMessage::new("user", "list files"));
        r.messages.push(ChatMessage {
            role: "assistant".into(),
            content: "thinking".into(),
            tool_calls: Some(vec![ToolCall {
                id: "tu_1".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            }]),
            tool_call_id: None,
            images: Vec::new(),
        });
        r.messages.push(ChatMessage {
            role: "tool".into(),
            content: "a.rs".into(),
            tool_calls: None,
            tool_call_id: Some("tu_1".into()),
            images: Vec::new(),
        });
        let resp = p.complete(&r).await.unwrap();
        assert_eq!(resp.content, "done");
    }

    #[tokio::test]
    async fn complete_requires_max_tokens() {
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
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "k");
        let mut s = p.stream(&req()).await.unwrap();
        let mut out = String::new();
        while let Some(ev) = s.next().await {
            if let StreamEvent::Text(t) = ev.unwrap() {
                out.push_str(&t);
            }
        }
        assert_eq!(out, "Hello");
    }

    #[tokio::test]
    async fn stream_yields_tool_call_event() {
        let server = MockServer::start().await;
        let sse = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"read\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\".rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .to_string();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "k");
        let mut s = p.stream(&req()).await.unwrap();
        let mut tcs = Vec::new();
        while let Some(ev) = s.next().await {
            if let StreamEvent::ToolCall(tc) = ev.unwrap() {
                tcs.push(tc);
            }
        }
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "tu_1");
        assert_eq!(tcs[0].name, "read");
        assert_eq!(tcs[0].arguments, "{\"path\":\"a.rs\"}");
    }
}
