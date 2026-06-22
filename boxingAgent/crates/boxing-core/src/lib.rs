//! boxingAgent agent loop（工具迭代 + 状态持久化）。
//!
//! 持有默认工具集，每轮把 tools 发给 provider；若返回 tool_calls 则派发、
//! 回填结果、迭代，直到模型不再调用工具或达到 max_turns。`on_delta` 流式
//! 渲染文本，`on_event` 渲染工具调用/结果。
//!
//! Phase 2d-2：通过 `with_store` 传入 `SessionStore` 后，loop 会在每次 run
//! 开头 `create_session`，并将每条 user/assistant/tool 消息写入 state.db。
//! 未传 store 时行为与 2d-1 一致（ephemeral）。

use boxing_providers::{
    ChatMessage, ChatRequest, Provider, ProviderError, StreamEvent, ToolCall, ToolDef,
};
use boxing_state::{MessageRecord, SessionStore};
use boxing_tools::Tool;
use futures::StreamExt;
use std::sync::{Arc, Mutex};

pub mod delegate;
pub use delegate::Delegate;

pub struct Agent {
    provider: Arc<dyn Provider>,
    model: String,
    system: String,
    tools: Vec<Box<dyn Tool>>,
    max_turns: usize,
    store: Option<Mutex<SessionStore>>,
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

/// Hermes 格式的工具调用持久化结构。
#[derive(serde::Serialize)]
struct PersistedToolCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        system: String,
        tools: Vec<Box<dyn Tool>>,
        max_turns: usize,
    ) -> Self {
        Self { provider, model, system, tools, max_turns, store: None }
    }

    /// 启用状态持久化（builder 模式）。
    pub fn with_store(mut self, store: SessionStore) -> Self {
        self.store = Some(Mutex::new(store));
        self
    }

    /// 为测试用：取出 store（便于断言持久化结果）。
    pub fn take_store(&mut self) -> Option<SessionStore> {
        self.store.take().map(|m| m.into_inner().expect("Mutex poisoned"))
    }

    /// 工具调用循环。返回最终回答（中间轮文本已通过 `on_delta` 流式输出）。
    /// 状态写入失败时传播错误。
    pub async fn run(
        &mut self,
        user_message: &str,
        on_delta: &mut impl FnMut(&str),
        on_event: &mut impl FnMut(LoopEvent),
    ) -> anyhow::Result<String> {
        let session_id = uuid::Uuid::new_v4().to_string();

        if let Some(store) = &self.store {
            store
                .lock()
                .unwrap()
                .create_session(&session_id, "cli", Some(&self.model), Some(&self.system))?;
        }

        let mut messages: Vec<ChatMessage> = Vec::new();
        if !self.system.is_empty() {
            let sys_msg = ChatMessage::new("system", self.system.as_str());
            persist_msg(&self.store, &session_id, &sys_msg)?;
            messages.push(sys_msg);
        }
        let user_msg = ChatMessage::new("user", user_message);
        persist_msg(&self.store, &session_id, &user_msg)?;
        messages.push(user_msg);

        for _ in 0..self.max_turns {
            let turn = self.step(&messages, on_delta).await?;
            let has_tools = !turn.tool_calls.is_empty();

            let assistant_msg = ChatMessage {
                role: "assistant".into(),
                content: turn.text.clone(),
                tool_calls: if has_tools {
                    Some(turn.tool_calls.clone())
                } else {
                    None
                },
                tool_call_id: None,
            };
            persist_msg(&self.store, &session_id, &assistant_msg)?;
            messages.push(assistant_msg);

            if !has_tools {
                return Ok(turn.text);
            }

            for tc in turn.tool_calls {
                on_event(LoopEvent::ToolCall { name: tc.name.clone() });
                let result = self.dispatch(&tc).await;
                let ok = !result.starts_with("error:");
                on_event(LoopEvent::ToolResult { name: tc.name.clone(), ok });

                let tool_msg = ChatMessage {
                    role: "tool".into(),
                    content: result,
                    tool_calls: None,
                    tool_call_id: Some(tc.id),
                };
                persist_msg(&self.store, &session_id, &tool_msg)?;
                messages.push(tool_msg);
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
                    description: s
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    parameters: s
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
                }
            })
            .collect()
    }
}

/// 把一条 ChatMessage 持久化到 state.db。
/// tool_calls 序列化为 Hermes 格式的 [{name, arguments}]。
fn persist_msg(
    store: &Option<Mutex<SessionStore>>,
    session_id: &str,
    msg: &ChatMessage,
) -> anyhow::Result<()> {
    let store = match store {
        Some(s) => s,
        None => return Ok(()),
    };
    let mut guard = store.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
    let tool_calls_json = msg.tool_calls.as_ref().map(|tcs| {
        let wire: Vec<_> = tcs
            .iter()
            .map(|tc| PersistedToolCall {
                name: tc.name.as_str(),
                arguments: tc.arguments.as_str(),
            })
            .collect();
        serde_json::to_string(&wire).unwrap_or_default()
    });
    let mut rec = MessageRecord::new(session_id, &msg.role);
    rec.content = Some(&msg.content);
    rec.tool_calls = tool_calls_json.as_deref();
    rec.tool_call_id = msg.tool_call_id.as_deref();
    guard.append_message(&rec)?;
    Ok(())
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
        async fn stream(
            &self,
            _: &ChatRequest,
        ) -> Result<boxing_providers::ChatStream, ProviderError> {
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
        let mut agent =
            Agent::new(Arc::new(provider), "m".into(), "".into(), vec![Box::new(Bash)], 5);
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
        let mut agent =
            Agent::new(Arc::new(provider), "m".into(), "".into(), vec![Box::new(Bash)], 2);
        let mut events = Vec::new();
        let answer = agent.run("go", &mut |_| {}, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(answer, "");
        assert!(events.iter().any(|e| matches!(e, LoopEvent::MaxTurns)));
    }

    /// 构建临时 schema_db（复制自 boxing-state 的测试 DDL）。
    fn schema_db() -> String {
        let dir = std::env::temp_dir().join(format!(
            "boxing-core-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (\
               id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, system_prompt TEXT,\
               started_at REAL NOT NULL, message_count INTEGER DEFAULT 0,\
               tool_call_count INTEGER DEFAULT 0, title TEXT);\
             CREATE TABLE messages (\
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,\
               role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT,\
               tool_name TEXT, timestamp REAL NOT NULL, token_count INTEGER,\
               finish_reason TEXT, observed INTEGER DEFAULT 0,\
               active INTEGER NOT NULL DEFAULT 1);",
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn loop_persists_messages_to_state_db() {
        // ScriptedProvider: turn 1 = tool_call(bash), turn 2 = text "done".
        let provider = ScriptedProvider {
            scripts: vec![
                vec![StreamEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"echo ok\"}".into(),
                })],
                vec![StreamEvent::Text("done".into())],
            ],
            call: Mutex::new(0),
        };
        let store = boxing_state::SessionStore::open(std::path::Path::new(&schema_db())).unwrap();
        let mut agent = Agent::new(
            Arc::new(provider),
            "m".into(),
            "SYS".into(),
            vec![Box::new(Bash)],
            5,
        )
        .with_store(store);
        let answer = agent.run("do it", &mut |_| {}, &mut |_| {}).await.unwrap();
        assert_eq!(answer, "done");

        // 验证 state.db：1 个 session，5 条消息
        // system + user + assistant(tool_calls) + tool + assistant(final)
        let store = agent.take_store().unwrap();
        assert_eq!(store.session_count().unwrap(), 1);
        let summaries = store.session_summaries().unwrap();
        assert_eq!(summaries[0].message_count, Some(5));
    }
}
