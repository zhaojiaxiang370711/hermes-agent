//! `delegate_task` 工具：子代理委托。
//!
//! 忠实移植 Hermes `delegate_tool.py` 的 `_run_single_child` 模式：
//! 创建独立的子 Agent（新对话、受限工具集），运行至完成，返回结果摘要。
//!
//! 快照角色 leaf（默认）阻止递归委托 + memory 工具（匹配 Hermes 阻止列表）。
//! `depth` 字段为 forward-compat（orchestrator 角色 + 嵌套委托 = 3b-2）。

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use boxing_providers::Provider;
use boxing_tools::{Tool, ToolError};

use crate::Agent;

/// leaf 角色子代理阻止的工具名称（匹配 Hermes 原版）。
const BLOCKED_TOOLS: &[&str] = &["delegate_task", "memory"];

/// 委托结果（匹配 Hermes 原版格式）。
#[derive(Serialize)]
struct DelegateResult {
    status: String,
    summary: Option<String>,
    error: Option<String>,
    duration_seconds: f64,
    exit_reason: String,
}

/// `delegate_task` 工具：子代理委托。
pub struct Delegate {
    provider: Arc<dyn Provider>,
    model: String,
    system: String,
    max_turns: usize,
    max_tokens: u32,
    /// 当前委托深度（0 = 父级，1 = 子代理）。
    depth: usize,
    /// 异步委托注册表（用于 background=true 模式）。
    async_registry: Option<Arc<crate::async_delegation::AsyncDelegationRegistry>>,
}

impl Delegate {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        system: String,
        max_turns: usize,
        max_tokens: u32,
        depth: usize,
    ) -> Self {
        Self {
            provider,
            model,
            system,
            max_turns,
            max_tokens,
            depth,
            async_registry: None,
        }
    }

    /// 设置异步委托注册表（启用 background=true 模式）。
    pub fn with_async_registry(
        mut self,
        registry: Arc<crate::async_delegation::AsyncDelegationRegistry>,
    ) -> Self {
        self.async_registry = Some(registry);
        self
    }

    /// 异步委托：后台运行子代理，立即返回 delegation_id。
    pub async fn exec_background(
        &self,
        user_message: String,
        child_tools: Vec<Box<dyn boxing_tools::Tool>>,
    ) -> Result<String, ToolError> {
        let registry = self.async_registry.as_ref().ok_or_else(|| {
            ToolError::Other("异步委托注册表未配置，请使用同步委托（background=false）".into())
        })?;

        let uuid_str = uuid::Uuid::new_v4().to_string();
        let delegation_id = format!("deleg-{}", &uuid_str[..8]);

        registry.dispatch(
            delegation_id.clone(),
            Arc::clone(&self.provider),
            self.model.clone(),
            self.system.clone(),
            child_tools,
            self.max_turns,
            self.max_tokens,
            user_message,
            None,
        );

        let result = serde_json::json!({
            "delegation_id": delegation_id,
            "status": "dispatched",
            "message": "子代理已在后台启动，完成后结果将自动注入对话。"
        });

        serde_json::to_string_pretty(&result).map_err(|e| ToolError::Other(e.to_string()))
    }
}

#[async_trait::async_trait]
impl Tool for Delegate {
    fn name(&self) -> &'static str {
        "delegate_task"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "delegate_task",
            "description":
                "Spawn a subagent to work on a task in an isolated context. \
                 The subagent gets its own conversation and toolset. \
                 Only the final summary is returned — intermediate tool results \
                 never enter your context window.\n\n\
                 Leaf subagents CANNOT call delegate_task or memory.\n\
                 Subagents have NO memory of your conversation — pass all \
                 relevant info via the 'context' field.",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "What the subagent should accomplish. \
                             Be specific and self-contained."
                    },
                    "context": {
                        "type": "string",
                        "description": "Background info the subagent needs: \
                             file paths, errors, constraints."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["leaf", "orchestrator"],
                        "description": "Child role. 'leaf' (default) cannot \
                             delegate further. 'orchestrator' is deferred."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run asynchronously in background. \
                             Returns delegation_id immediately."
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
        let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("leaf");
        let background = args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 角色校验：3b-1 只支持 leaf
        if role != "leaf" {
            return Err(ToolError::InvalidArg {
                arg: "role",
                reason: "orchestrator 角色在 3b-1 中暂不支持，请使用 leaf".into(),
            });
        }

        // 深度校验（forward-compat，leaf 子代理无法触发）
        if self.depth >= 1 {
            return Err(ToolError::Other("委托深度超限（max_spawn_depth=1）".into()));
        }

        // 子代理工具集：过滤掉被阻止的工具
        let child_tools: Vec<Box<dyn boxing_tools::Tool>> = boxing_tools::default_tools()
            .into_iter()
            .filter(|t| !BLOCKED_TOOLS.contains(&t.name()))
            .collect();

        // 组装子代理的用户消息（context 追加到 goal 前）
        let user_message = match context {
            Some(ctx) => format!("{ctx}\n\n{goal}"),
            None => goal,
        };

        // background=true：异步委托（立即返回 delegation_id）
        if background {
            return self.exec_background(user_message, child_tools).await;
        }

        // 同步委托：创建子代理并运行至完成
        let mut child = Agent::new(
            Arc::clone(&self.provider),
            self.model.clone(),
            self.system.clone(),
            child_tools,
            self.max_turns,
            self.max_tokens,
        );

        let start = Instant::now();
        // 运行子代理（捕获文本，不流式输出到父代理 stdout）
        let result = child
            .run(&user_message, &mut |_delta| {}, &mut |_ev| {})
            .await;
        let duration = start.elapsed().as_secs_f64();

        let out = match result {
            Ok(answer) => DelegateResult {
                status: "completed".into(),
                summary: Some(answer),
                error: None,
                duration_seconds: duration,
                exit_reason: "completed".into(),
            },
            Err(e) => DelegateResult {
                status: "failed".into(),
                summary: None,
                error: Some(e.to_string()),
                duration_seconds: duration,
                exit_reason: "error".into(),
            },
        };
        serde_json::to_string_pretty(&out).map_err(|e| ToolError::Other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxing_providers::{ChatRequest, ChatResponse, ChatStream, ProviderError, StreamEvent};
    use serde_json::json;

    /// 脚本化 provider：直接返回预设文本。
    struct StaticProvider(String);
    #[async_trait::async_trait]
    impl boxing_providers::Provider for StaticProvider {
        async fn complete(&self, _: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            unreachable!()
        }
        async fn stream(&self, _: &ChatRequest) -> Result<ChatStream, ProviderError> {
            let text = self.0.clone();
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Text(text),
            )])))
        }
    }

    #[tokio::test]
    async fn delegate_runs_child_and_returns_result() {
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider("子代理回答".into()));
        let delegate = Delegate::new(provider, "m".into(), "".into(), 5, 4096, 0);
        let out = delegate.exec(json!({"goal": "执行任务"})).await.unwrap();
        assert!(out.contains("completed"));
        assert!(out.contains("子代理回答"));
        assert!(out.contains("duration_seconds"));
    }

    #[tokio::test]
    async fn orchestrator_role_rejected() {
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider("".into()));
        let delegate = Delegate::new(provider, "m".into(), "".into(), 5, 4096, 0);
        let err = delegate
            .exec(json!({"goal": "x", "role": "orchestrator"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArg { .. }));
    }

    #[tokio::test]
    async fn child_blocked_from_memory_and_delegate() {
        // 子代理工具集不包含 delegate_task 和 memory
        let child_tools: Vec<Box<dyn boxing_tools::Tool>> = boxing_tools::default_tools()
            .into_iter()
            .filter(|t| !BLOCKED_TOOLS.contains(&t.name()))
            .collect();
        let names: Vec<&str> = child_tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"memory"), "子代理不应包含 memory");
        assert!(
            !names.contains(&"delegate_task"),
            "子代理不应包含 delegate_task"
        );
        assert!(names.contains(&"read"), "子代理应包含 read");
    }
}
