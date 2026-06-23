//! 异步委托模块。
//!
//! 实现 Hermes 原版的 background=true 模式：
//! 1. 子代理在 tokio 后台任务中运行
//! 2. 父代理立即返回 delegation_id，不阻塞
//! 3. 子代理完成后，结果可通过 completion_queue 获取
//!
//! 与 Hermes 的差异：Rust 使用 tokio::spawn 替代 ThreadPoolExecutor，
//! 使用 tokio::sync::mpsc 替代 Python 的 queue。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use boxing_providers::Provider;
use boxing_tools::{Tool, ToolError};

use crate::Agent;

/// 异步委托结果。
#[derive(Serialize)]
pub struct AsyncDelegationResult {
    pub delegation_id: String,
    pub status: String, // "dispatched"
    pub message: String,
}

/// 完成的委托结果（从 completion_queue 接收）。
#[derive(Debug, Serialize)]
pub struct CompletedDelegation {
    pub delegation_id: String,
    pub status: String, // "completed" | "failed"
    pub summary: Option<String>,
    pub error: Option<String>,
    pub duration_seconds: f64,
    pub exit_reason: String,
}

/// 异步委托注册表：跟踪后台任务。
pub struct AsyncDelegationRegistry {
    /// delegation_id -> JoinHandle (用于中断/查询)
    tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// 完成队列发送端
    tx: mpsc::Sender<CompletedDelegation>,
    /// 完成队列接收端（由调用方轮询），使用 tokio::sync::Mutex 保护
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<CompletedDelegation>>>,
}

impl Default for AsyncDelegationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncDelegationRegistry {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(100);
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            tx,
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }

    /// 分派一个异步委托任务。
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch(
        &self,
        delegation_id: String,
        provider: Arc<dyn Provider>,
        model: String,
        system: String,
        tools: Vec<Box<dyn Tool>>,
        max_turns: usize,
        max_tokens: u32,
        goal: String,
        context: Option<String>,
    ) {
        let tx = self.tx.clone();
        let id = delegation_id.clone();

        let handle = tokio::spawn(async move {
            let start = Instant::now();
            let mut agent = Agent::new(provider, model, system, tools, max_turns, max_tokens);

            // 组装用户消息（context 追加到 goal 前）
            let user_message = match context {
                Some(ctx) => format!("{ctx}\n\n{goal}"),
                None => goal,
            };

            // 运行子代理（静默，不流式输出）
            let result = agent
                .run(&user_message, &mut |_delta| {}, &mut |_ev| {})
                .await;

            let duration = start.elapsed().as_secs_f64();

            let completed = match result {
                Ok(summary) => CompletedDelegation {
                    delegation_id: id.clone(),
                    status: "completed".to_string(),
                    summary: Some(summary),
                    error: None,
                    duration_seconds: duration,
                    exit_reason: "completed".to_string(),
                },
                Err(e) => CompletedDelegation {
                    delegation_id: id.clone(),
                    status: "failed".to_string(),
                    summary: None,
                    error: Some(e.to_string()),
                    duration_seconds: duration,
                    exit_reason: "error".to_string(),
                },
            };

            // 发送到完成队列
            let _ = tx.send(completed).await;
        });

        // 注册任务句柄
        self.tasks.lock().unwrap().insert(delegation_id, handle);
    }

    /// 尝试从完成队列接收一个已完成的委托（非阻塞）。
    pub async fn try_recv(&self) -> Option<CompletedDelegation> {
        self.rx.lock().await.try_recv().ok()
    }

    /// 阻塞等待一个已完成的委托。
    pub async fn recv(&self) -> Option<CompletedDelegation> {
        self.rx.lock().await.recv().await
    }

    /// 获取当前活跃的后台任务数。
    pub fn active_count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    /// 中断一个后台任务。
    pub fn abort(&self, delegation_id: &str) -> bool {
        if let Some(handle) = self.tasks.lock().unwrap().remove(delegation_id) {
            handle.abort();
            true
        } else {
            false
        }
    }
}

/// `delegate_task` 工具：支持异步后台委托。
pub struct AsyncDelegate {
    provider: Arc<dyn Provider>,
    model: String,
    system: String,
    max_turns: usize,
    max_tokens: u32,
    _depth: usize,
    registry: Arc<AsyncDelegationRegistry>,
}

impl AsyncDelegate {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        system: String,
        max_turns: usize,
        max_tokens: u32,
        depth: usize,
        registry: Arc<AsyncDelegationRegistry>,
    ) -> Self {
        Self {
            provider,
            model,
            system,
            max_turns,
            max_tokens,
            _depth: depth,
            registry,
        }
    }
}

#[async_trait::async_trait]
impl Tool for AsyncDelegate {
    fn name(&self) -> &'static str {
        "delegate_task_async"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "delegate_task_async",
            "description": "异步委托：后台运行子代理，立即返回 delegation_id。子代理完成后结果可通过 completion_queue 获取。",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "子代理要完成的任务目标。"
                    },
                    "context": {
                        "type": "string",
                        "description": "子代理需要的背景信息。"
                    }
                },
                "required": ["goal"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let goal = args
            .get("goal")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("goal"))?
            .to_string();

        let context = args
            .get("context")
            .and_then(|v| v.as_str())
            .map(String::from);

        // 生成 delegation_id
        let uuid_str = uuid::Uuid::new_v4().to_string();
        let delegation_id = format!("deleg-{}", &uuid_str[..8]);

        // 获取子代理工具集（排除 delegate_task 和 memory）
        let child_tools: Vec<Box<dyn Tool>> = boxing_tools::default_tools()
            .into_iter()
            .filter(|t| {
                t.name() != "delegate_task"
                    && t.name() != "delegate_task_async"
                    && t.name() != "memory"
            })
            .collect();

        // 分派异步任务
        self.registry.dispatch(
            delegation_id.clone(),
            Arc::clone(&self.provider),
            self.model.clone(),
            self.system.clone(),
            child_tools,
            self.max_turns,
            self.max_tokens,
            goal,
            context,
        );

        // 立即返回 delegation_id
        let result = AsyncDelegationResult {
            delegation_id,
            status: "dispatched".to_string(),
            message: "子代理已在后台启动，完成后结果将自动注入对话。".to_string(),
        };

        serde_json::to_string_pretty(&result).map_err(|e| ToolError::Other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxing_providers::{
        ChatRequest, ChatResponse, ChatStream, ProviderError, StreamEvent, Usage,
    };

    struct MockProvider;
    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn complete(&self, _req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse {
                content: "test".into(),
                usage: Usage::default(),
                tool_calls: vec![],
            })
        }
        async fn stream(&self, _req: &ChatRequest) -> Result<ChatStream, ProviderError> {
            use futures::stream;
            Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::Text(
                "test".into(),
            ))])))
        }
    }

    #[tokio::test]
    async fn async_delegate_returns_immediately() {
        let registry = Arc::new(AsyncDelegationRegistry::new());
        let delegate = AsyncDelegate::new(
            Arc::new(MockProvider),
            "m".into(),
            "".into(),
            5,
            4096,
            0,
            Arc::clone(&registry),
        );

        let result = delegate.exec(json!({"goal": "test task"})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "dispatched");
        assert!(parsed["delegation_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn completion_queue_receives_result() {
        let registry = Arc::new(AsyncDelegationRegistry::new());
        let delegate = AsyncDelegate::new(
            Arc::new(MockProvider),
            "m".into(),
            "".into(),
            5,
            4096,
            0,
            Arc::clone(&registry),
        );

        let result = delegate.exec(json!({"goal": "test"})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let _delegation_id = parsed["delegation_id"].as_str().unwrap();

        // 等待完成
        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), registry.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(completed.status, "completed");
        assert!(completed.summary.is_some());
    }
}
