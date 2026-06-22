//! `read` 工具：读取文本文件，按 `cat -n` 风格带行号返回。

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::{arg_optional_u64, arg_str, Tool, ToolError};

/// 默认最多返回行数。
const DEFAULT_LIMIT: u64 = 2000;

pub struct Read;

#[async_trait::async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "read",
            "description": "读取文本文件，返回带行号的行（cat -n 风格）。大文件用 offset/limit 分页。",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "相对 cwd 的文件路径（或绝对路径）"},
                    "offset": {"type": "integer", "description": "起始行号（1 基，默认 1）", "default": 1},
                    "limit": {"type": "integer", "description": "最多返回行数（默认 2000）", "default": 2000}
                },
                "required": ["path"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let path = arg_str(&args, "path")?;
        let offset = arg_optional_u64(&args, "offset").unwrap_or(1).max(1);
        let limit = arg_optional_u64(&args, "limit").unwrap_or(DEFAULT_LIMIT);

        let p = Path::new(&path);
        if !p.exists() {
            return Err(ToolError::Other(format!("文件不存在: {path}")));
        }
        if p.is_dir() {
            return Err(ToolError::Other(format!("是目录而非文件: {path}")));
        }
        let bytes = std::fs::read(p)?;
        if bytes.contains(&0u8) {
            return Err(ToolError::Other(format!("二进制文件: {path}")));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let start = ((offset - 1) as usize).min(total); // 1 基行号 → 0 基索引
        let end = (start + limit as usize).min(total);

        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let _ = writeln!(out, "{:>6}\t{}", start + i + 1, line);
        }
        if end < total {
            let _ = writeln!(out, "<{} 行更多>", total - end);
        }
        if out.is_empty() {
            out.push_str("<空或越界范围>");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str, body: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "boxing-read-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn reads_with_line_numbers_and_respects_limit() {
        let p = tmp("limit", "a\nb\nc\nd\ne\n");
        let out = Read
            .exec(json!({"path": p, "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(out.contains("     2\tb"), "got:\n{out}");
        assert!(out.contains("     3\tc"), "got:\n{out}");
        assert!(out.contains("<2 行更多>"));
        assert!(!out.contains("\td"));
    }

    #[tokio::test]
    async fn errors_on_missing_and_directory_and_binary() {
        assert!(matches!(
            Read.exec(json!({"path": "/no/such/boxing-xyz"})).await,
            Err(ToolError::Other(_))
        ));
        let dir = std::env::temp_dir().join(format!("boxing-read-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Read.exec(json!({"path": dir.to_string_lossy()})).await.is_err());

        let p = tmp("bin", "a\x00b\n");
        assert!(Read.exec(json!({"path": p})).await.is_err());
    }
}
