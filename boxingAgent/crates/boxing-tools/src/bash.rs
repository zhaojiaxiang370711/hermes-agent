//! `bash` 工具：在 cwd 用 `sh -c` 执行命令，捕获 stdout/stderr/退出码，带超时。
//!
//! 超时则杀死子进程并返回 [`ToolError::Timeout`]。输出超长则截断。

use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::{arg_optional_u64, arg_str, Tool, ToolError};

/// 默认超时（秒）。
const DEFAULT_TIMEOUT: u64 = 120;
/// 输出硬上限（字符数）。
const MAX_OUTPUT: usize = 100_000;

pub struct Bash;

#[async_trait::async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "bash",
            "description": "在 cwd 执行 shell 命令（sh -c），返回 stdout、stderr 与退出码。默认 120s 超时。",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "description": "超时秒数（默认 120）", "default": 120}
                },
                "required": ["command"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let command = arg_str(&args, "command")?;
        let timeout = arg_optional_u64(&args, "timeout").unwrap_or(DEFAULT_TIMEOUT);

        // 绑定 cmd，避免对临时 Command 调用 &mut self 的 output() 触发借用期问题。
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true); // 超时后确保子进程被杀

        let output = tokio::time::timeout(Duration::from_secs(timeout), cmd.output())
            .await
            .map_err(|_| ToolError::Timeout(timeout))??; // spawn + wait，返回 Output

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        let mut out = String::new();
        out.push_str(&stdout);
        if !stderr.is_empty() {
            out.push_str("\nstderr:\n");
            out.push_str(&stderr);
        }
        let _ = writeln!(out, "\nexit: {code}");

        if out.len() > MAX_OUTPUT {
            out.truncate(MAX_OUTPUT);
            out.push_str("\n<输出已截断>");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn captures_stdout_and_exit_zero() {
        let out = Bash.exec(json!({"command": "echo hi"})).await.unwrap();
        assert!(out.contains("hi"));
        assert!(out.contains("exit: 0"));
    }

    #[tokio::test]
    async fn captures_stderr_and_nonzero_exit() {
        let out = Bash
            .exec(json!({"command": "echo err >&2; exit 3"}))
            .await
            .unwrap();
        assert!(out.contains("stderr:"));
        assert!(out.contains("err"));
        assert!(out.contains("exit: 3"));
    }

    #[tokio::test]
    async fn times_out() {
        let err = Bash
            .exec(json!({"command": "sleep 5", "timeout": 1}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(1)));
    }
}
