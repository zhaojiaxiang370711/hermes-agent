//! boxingAgent 默认工具集（Phase 2a）。
//!
//! 定义统一的 [`Tool`] trait 与 [`ToolError`]，以及 7 个精简编码工具：
//! read / write / edit / bash / grep / glob / ls。每个工具一个模块，
//! 经 [`default_tools`]（后续任务补全）一次性获取全部。

use serde_json::Value;

pub mod read;
pub mod write;

pub use read::Read;
pub use write::Write;

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
    fn name(&self) -> &'static str;
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
