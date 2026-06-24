//! 火山（豆包）TTS provider — WebSocket 单向流式。完整实现见 plan Task 4。

use std::path::Path;

use super::VolcanoCfg;
use crate::ToolError;

/// 占位实现：Task 4 替换为真实 WS 客户端。
pub async fn generate(
    _text: &str,
    _out: &Path,
    _cfg: &VolcanoCfg,
    _voice: &str,
    _env_path: &Path,
) -> Result<(), ToolError> {
    Err(ToolError::Other(
        "volcano provider 尚未实现（见 plan Task 4）".into(),
    ))
}
