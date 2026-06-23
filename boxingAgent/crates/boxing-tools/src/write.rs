//! `write` 工具：创建或覆盖文件，自动建父目录。

use std::path::Path;

use serde_json::{json, Value};

use crate::{arg_str, Tool, ToolError};

pub struct Write;

#[async_trait::async_trait]
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "write",
            "description": "用给定内容创建或覆盖文件；自动创建父目录。",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let path = arg_str(&args, "path")?;
        let content = arg_str(&args, "content")?;
        let p = Path::new(&path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?; // 自动建父目录
        }
        let n = content.len();
        std::fs::write(p, &content)?;
        Ok(format!("已写入 {n} 字节到 {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn writes_creates_parents_and_overwrites() {
        let dir = std::env::temp_dir().join(format!(
            "boxing-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let p = dir.join("sub/f.txt");
        let out = Write
            .exec(json!({"path": p.to_string_lossy(), "content": "hello"}))
            .await
            .unwrap();
        assert!(out.contains("已写入 5 字节"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        // 覆盖
        Write
            .exec(json!({"path": p.to_string_lossy(), "content": "hi"}))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi");
    }

    #[tokio::test]
    async fn missing_required_arg_errors() {
        assert!(matches!(
            Write.exec(json!({"path": "/x"})).await,
            Err(ToolError::MissingArg(_))
        ));
    }
}
