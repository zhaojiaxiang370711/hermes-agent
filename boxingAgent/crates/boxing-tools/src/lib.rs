//! boxingAgent 默认工具集（Phase 2a + Phase 3）。
//!
//! 定义统一的 [`Tool`] trait 与 [`ToolError`]，以及 11 个工具：
//! read / write / edit / bash / grep / glob / ls / clarify / todo / memory / session_search。
//! 每个工具一个模块，经 [`default_tools`] 一次性获取全部。

use serde_json::Value;

pub mod bash;
pub mod clarify;
pub mod code_execution;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod image_generate;
pub mod ls;
pub mod mcp;
pub mod memory;
pub mod oauth;
pub mod read;
pub mod search;
pub mod todo;
pub mod vision;
pub mod web;
pub mod write;

pub use bash::Bash;
pub use clarify::Clarify;
pub use code_execution::ExecuteCode;
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use image_generate::ImageGenerate;
pub use ls::Ls;
pub use memory::Memory;
pub use read::Read;
pub use search::SessionSearch;
pub use todo::Todo;
pub use vision::Vision;
pub use web::WebSearch;
pub use write::Write;

/// 返回全部 15 个默认工具。
pub fn default_tools() -> Vec<Box<dyn Tool>> {
    let home = hermes_home();
    vec![
        Box::new(Read),
        Box::new(Write),
        Box::new(Edit),
        Box::new(Bash),
        Box::new(Grep),
        Box::new(Glob),
        Box::new(Ls),
        Box::new(Clarify),
        Box::new(Vision),
        Box::new(WebSearch),
        Box::new(ImageGenerate::new()),
        Box::new(ExecuteCode::new(vec![
            "read_file".into(),
            "write_file".into(),
            "edit".into(),
            "bash".into(),
            "grep".into(),
            "glob".into(),
            "ls".into(),
        ])),
        Box::new(Todo::new()),
        Box::new(Memory::new(&home)),
        Box::new(SessionSearch::new(home.join("state.db"))),
    ]
}

/// 获取 `~/.hermes` 路径（复用 Hermes 的 HOME 逻辑）。
pub fn hermes_home() -> std::path::PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let h = std::env::var("HOME").expect("HOME not set");
            std::path::PathBuf::from(h).join(".hermes")
        })
}

/// 工具错误类型；所有工具的 exec 统一返回 `Result<String, ToolError>`。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("缺少必需参数: {0}")]
    MissingArg(&'static str),
    #[error("参数 {arg} 无效: {reason}")]
    InvalidArg { arg: &'static str, reason: String },
    #[error("io 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("正则错误: {0}")]
    Regex(#[from] regex::Error),
    #[error("命令在 {0}s 后超时")]
    Timeout(u64),
    #[error("{0}")]
    Other(String),
}

/// 工具 trait：模型可调用的统一接口。
///
/// - [`Tool::name`]：稳定工具名（如 "read"）。
/// - [`Tool::schema`]：OpenAI function-calling 风格的 JSON Schema。
/// - [`Tool::exec`]：以解析后的参数执行，返回模型可见的文本。
///
/// 工具基于进程 cwd 运行；本阶段无 ToolContext / 沙箱 / 审批。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    async fn exec(&self, args: Value) -> Result<String, ToolError>;
}

// ===== 参数解析辅助（crate 内部） =====

/// 必需字符串参数；缺失返回 `MissingArg`。
pub(crate) fn arg_str(args: &Value, key: &'static str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or(ToolError::MissingArg(key))
}

pub(crate) fn arg_optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub(crate) fn arg_optional_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

pub(crate) fn arg_optional_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Dummy;
    #[async_trait::async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn schema(&self) -> Value {
            json!({})
        }
        async fn exec(&self, _args: Value) -> Result<String, ToolError> {
            Ok("ok".into())
        }
    }

    #[tokio::test]
    async fn tool_is_object_safe() {
        let t: Box<dyn Tool> = Box::new(Dummy);
        assert_eq!(t.name(), "dummy");
        assert_eq!(t.exec(json!({})).await.unwrap(), "ok");
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    /// 实现的工具集必须恰好是 catalog 中的 15 个，名字一致。
    #[test]
    fn default_tools_match_catalog() {
        const CATALOG: &str = include_str!("../../../specs/tools-phase2a.yaml");
        let tools = default_tools();
        let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        let expected = [
            "read",
            "write",
            "edit",
            "bash",
            "grep",
            "glob",
            "ls",
            "clarify",
            "vision",
            "web_search",
            "image_generate",
            "execute_code",
            "todo",
            "memory",
            "session_search",
        ];
        assert_eq!(names, expected);
        for n in expected {
            assert!(
                CATALOG.contains(&format!("- name: {n}")),
                "catalog 缺少工具 {n}"
            );
        }
    }
}
