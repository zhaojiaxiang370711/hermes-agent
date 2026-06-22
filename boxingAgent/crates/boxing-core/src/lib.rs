//! boxingAgent agent loop（工具迭代）。
//!
//! 持有默认工具集，每轮把 tools 发给 provider；若返回 tool_calls 则派发、
//! 回填结果、迭代，直到模型不再调用工具或达到 max_turns。`on_delta` 流式
//! 渲染文本，`on_event` 渲染工具调用/结果。Phase 2d-1：ephemeral（不落 state）。

use boxing_providers::{
    ChatMessage, ChatRequest, Provider, ProviderError, StreamEvent, ToolCall, ToolDef,
};
use boxing_tools::Tool;
use futures::StreamExt;

pub struct Agent {
    provider: Box<dyn Provider>,
    model: String,
    system: String,
    tools: Vec<Box<dyn Tool>>,
    max_turns: usize,
}

/// 一轮 provider 调用的输出。
pub struct TurnOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// loop 事件（供调用方 live 渲染）。
#[derive(Debug, Clone)]
pub enum LoopEvent {
    ToolCall { name: String },
    ToolResult { name: String, ok: bool },
    MaxTurns,
}

impl Agent {
    pub fn new(
        provider: Box<dyn Provider>,
        model: String,
        system: String,
        tools: Vec<Box<dyn Tool>>,
        max_turns: usize,
    ) -> Self {
        Self { provider, model, system, tools, max_turns }
    }

    /// 工具调用循环。返回最终回答（中间轮文本已通过 `on_delta` 流式输出）。
    pub async fn run(
        &self,
        user_message: &str,
        on_delta: &mut impl FnMut(&str),
        on_event: &mut impl FnMut(LoopEvent),
    ) -> Result<String, ProviderError> {
        let mut messages: Vec<ChatMessage> = Vec::new();
        if !self.system.is_empty() {
            messages.push(ChatMessage::new("system", self.system.as_str()));
        }
        messages.push(ChatMessage::new("user", user_message));

        for _ in 0..self.max_turns {
            let turn = self.step(&messages, on_delta).await?;
            if turn.tool_calls.is_empty() {
                return Ok(turn.text);
            }
            // 记录 assistant 的工具调用消息
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: turn.text,
                tool_calls: Some(turn.tool_calls.clone()),
                tool_call_id: None,
            });
            // 逐个派发
            for tc in turn.tool_calls {
                on_event(LoopEvent::ToolCall { name: tc.name.clone() });
                let result = self.dispatch(&tc).await;
                let ok = !result.starts_with("error:");
                on_event(LoopEvent::ToolResult { name: tc.name.clone(), ok });
                messages.push(ChatMessage {
                    role: "tool".into(),
                    content: result,
                    tool_calls: None,
                    tool_call_id: Some(tc.id),
                });
            }
        }
        on_event(LoopEvent::MaxTurns);
        Ok(String::new())
    }

    /// 单轮：发 tools，流式取 TurnOutput。
    async fn step(
        &self,
        messages: &[ChatMessage],
        on_delta: &mut impl FnMut(&str),
    ) -> Result<TurnOutput, ProviderError> {
        let mut req = ChatRequest::new(self.model.as_str(), messages.to_vec());
        req.stream = true;
        req.tools = self.tool_defs();
        let mut stream = self.provider.stream(&req).await?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        while let Some(ev) = stream.next().await {
            match ev? {
                StreamEvent::Text(t) => {
                    on_delta(&t);
                    text.push_str(&t);
                }
                StreamEvent::ToolCall(tc) => tool_calls.push(tc),
            }
        }
        Ok(TurnOutput { text, tool_calls })
    }

    /// 派发一次工具调用：按 name 找工具，`exec(parse(arguments))`。
    async fn dispatch(&self, tc: &ToolCall) -> String {
        let args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
        match self.tools.iter().find(|t| t.name() == tc.name) {
            Some(t) => match t.exec(args).await {
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            },
            None => format!("error: 未知工具 {}", tc.name),
        }
    }

    /// 由 tools 的 `name()` + `schema()` 派生 ToolDef。
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| {
                let s = t.schema();
                ToolDef {
                    name: t.name().to_string(),
                    description: s.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    parameters: s
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxing_providers::ChatRequest;
    use boxing_tools::Bash;
    use std::sync::Mutex;

    /// 脚本化 provider：按调用序号返回预设的 StreamEvent 流。
    struct ScriptedProvider {
        scripts: Vec<Vec<StreamEvent>>,
        call: Mutex<usize>,
    }
    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            _: &ChatRequest,
        ) -> Result<boxing_providers::ChatResponse, ProviderError> {
            unreachable!()
        }
        async fn stream(&self, _: &ChatRequest) -> Result<boxing_providers::ChatStream, ProviderError> {
            let mut c = self.call.lock().unwrap();
            let i = *c;
            *c += 1;
            let evs = self
                .scripts
                .get(i)
                .cloned()
                .or_else(|| self.scripts.last().cloned())
                .unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(evs.into_iter().map(Ok))))
        }
    }

    #[tokio::test]
    async fn loop_dispatches_tool_then_finishes() {
        // 第 1 轮：tool_call(bash)；第 2 轮：纯文本 "done"。
        let provider = ScriptedProvider {
            scripts: vec![
                vec![StreamEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"echo hi\"}".into(),
                })],
                vec![StreamEvent::Text("done".into())],
            ],
            call: Mutex::new(0),
        };
        let agent = Agent::new(Box::new(provider), "m".into(), "".into(), vec![Box::new(Bash)], 5);
        let mut events = Vec::new();
        let answer = agent.run("do it", &mut |_| {}, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(answer, "done");
        assert!(events
            .iter()
            .any(|e| matches!(e, LoopEvent::ToolCall { name } if name == "bash")));
        assert!(events
            .iter()
            .any(|e| matches!(e, LoopEvent::ToolResult { name, ok } if name == "bash" && *ok)));
    }

    #[tokio::test]
    async fn max_turns_returns_empty() {
        // 每轮都发 tool_call，永不结束 → max_turns 截断。
        let provider = ScriptedProvider {
            scripts: vec![vec![StreamEvent::ToolCall(ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"true\"}".into(),
            })]],
            call: Mutex::new(0),
        };
        let agent = Agent::new(Box::new(provider), "m".into(), "".into(), vec![Box::new(Bash)], 2);
        let mut events = Vec::new();
        let answer = agent.run("go", &mut |_| {}, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(answer, "");
        assert!(events.iter().any(|e| matches!(e, LoopEvent::MaxTurns)));
    }
}
