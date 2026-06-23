//! `glob` 工具：按 glob 模式查找文件，按修改时间倒序返回。

use serde_json::{json, Value};

use crate::{arg_optional_str, arg_str, Tool, ToolError};

pub struct Glob;

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "glob",
            "description": "按 glob 模式查找文件（如 **/*.rs），按修改时间倒序返回。",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "default": "."}
                },
                "required": ["pattern"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let pattern = arg_str(&args, "pattern")?;
        let path = arg_optional_str(&args, "path").unwrap_or_else(|| ".".into());
        let full = format!("{}/{}", path.trim_end_matches('/'), pattern);

        let mut entries: Vec<(std::time::SystemTime, String)> = Vec::new();
        for entry in glob::glob(&full)
            .map_err(|e| ToolError::Other(e.to_string()))?
            .flatten()
        {
            let mtime = std::fs::metadata(&entry)
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((mtime, entry.to_string_lossy().into_owned()));
        }
        entries.sort_by_key(|a| std::cmp::Reverse(a.0)); // mtime 倒序
        Ok(entries
            .iter()
            .map(|(_, p)| p.as_str())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn matches_glob_pattern() {
        let dir = std::env::temp_dir().join(format!(
            "boxing-glob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "").unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        let out = Glob
            .exec(json!({"pattern": "*.rs", "path": dir.to_string_lossy()}))
            .await
            .unwrap();
        assert!(out.contains("a.rs"));
        assert!(!out.contains("b.txt"));
    }
}
