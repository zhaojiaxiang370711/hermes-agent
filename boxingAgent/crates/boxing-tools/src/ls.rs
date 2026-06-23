//! `ls` 工具：列出目录条目；目录名后缀 `/`。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{arg_optional_bool, arg_optional_str, Tool, ToolError};

pub struct Ls;

#[async_trait::async_trait]
impl Tool for Ls {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "ls",
            "description": "列出目录条目（目录后缀 /）。recursive 递归列出子目录。",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "default": "."},
                    "recursive": {"type": "boolean", "default": false}
                }
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let path = arg_optional_str(&args, "path").unwrap_or_else(|| ".".into());
        let recursive = arg_optional_bool(&args, "recursive").unwrap_or(false);

        let mut lines: Vec<String> = Vec::new();
        walk(Path::new(&path), PathBuf::new(), recursive, &mut lines)?;
        lines.sort();
        Ok(lines.join("\n"))
    }
}

/// 递归收集条目；`rel` 为相对根的路径前缀（根层时为空）。
fn walk(
    root: &Path,
    rel: PathBuf,
    recursive: bool,
    out: &mut Vec<String>,
) -> Result<(), ToolError> {
    for entry in std::fs::read_dir(root)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let path_str = rel.join(&name).display().to_string();
        out.push(if is_dir {
            format!("{path_str}/")
        } else {
            path_str
        });
        if recursive && is_dir {
            walk(&entry.path(), rel.join(&name), recursive, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> String {
        let dir = std::env::temp_dir().join(format!(
            "boxing-ls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn lists_with_dir_suffix() {
        let dir = fixture();
        let out = Ls.exec(json!({"path": dir})).await.unwrap();
        assert!(out.contains("a.txt"));
        assert!(out.contains("sub/"));
    }

    #[tokio::test]
    async fn recursive_walks_subdir() {
        let dir = fixture();
        std::fs::write(std::path::Path::new(&dir).join("sub/c.txt"), "").unwrap();
        let out = Ls
            .exec(json!({"path": dir, "recursive": true}))
            .await
            .unwrap();
        assert!(out.contains("sub/c.txt"));
    }
}
