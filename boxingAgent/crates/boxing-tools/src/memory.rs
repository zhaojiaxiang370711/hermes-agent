//! `memory` 工具：文件持久化笔记。
//!
//! 管理 `~/.hermes/MEMORY.md` 和 `~/.hermes/USER.md`（与 Hermes 原版共享，互操作）。
//! 条目用 `§` 分隔。Schema 与 Hermes `memory_tool.py` 一致。
//! 三种操作：add（追加）、replace（按 old_text 子串替换）、remove（按 old_text 删除）。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::{Tool, ToolError};

/// 条目分隔符（与 Hermes 原版一致）。
const DELIMITER: &str = "§";

/// `memory` 工具：管理 ~/.hermes/ 下的持久化笔记文件。
pub struct Memory {
    memory_path: PathBuf,
    user_path: PathBuf,
}

impl Memory {
    pub fn new(hermes_home: &Path) -> Self {
        Self {
            memory_path: hermes_home.join("MEMORY.md"),
            user_path: hermes_home.join("USER.md"),
        }
    }

    /// 根据 target 选择文件路径。
    fn target_path(&self, target: &str) -> Result<&PathBuf, ToolError> {
        match target {
            "memory" => Ok(&self.memory_path),
            "user" => Ok(&self.user_path),
            _ => Err(ToolError::InvalidArg {
                arg: "target",
                reason: format!("未知 target: {target}，可选 memory / user"),
            }),
        }
    }
}

#[async_trait::async_trait]
impl Tool for Memory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "memory",
            "description": "Save durable information to persistent memory. Two targets: 'memory' (agent notes) and 'user' (user profile). Actions: add (new entry), replace (update existing by old_text substring), remove (delete by old_text substring).",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                    "target": {"type": "string", "enum": ["memory", "user"]},
                    "content": {"type": "string", "description": "Entry content. Required for add and replace."},
                    "old_text": {"type": "string", "description": "Substring identifying the entry to replace/remove."}
                },
                "required": ["action", "target"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("action"))?;
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("target"))?;
        let path = self.target_path(target)?.clone();

        match action {
            "add" => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or(ToolError::MissingArg("content"))?;
                let mut entries = read_entries(&path);
                entries.push(content.to_string());
                write_entries(&path, &entries)?;
                Ok(format!("已添加到 {target}（共 {} 条）", entries.len()))
            }
            "replace" => {
                let old_text = args
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .ok_or(ToolError::MissingArg("old_text"))?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or(ToolError::MissingArg("content"))?;
                let mut entries = read_entries(&path);
                match entries.iter().position(|e| e.contains(old_text)) {
                    Some(i) => {
                        entries[i] = content.to_string();
                        write_entries(&path, &entries)?;
                        Ok(format!("已替换 {target} 中的条目"))
                    }
                    None => Err(ToolError::Other(format!(
                        "在 {target} 中未找到包含 '{old_text}' 的条目"
                    ))),
                }
            }
            "remove" => {
                let old_text = args
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .ok_or(ToolError::MissingArg("old_text"))?;
                let mut entries = read_entries(&path);
                let before = entries.len();
                entries.retain(|e| !e.contains(old_text));
                if entries.len() == before {
                    return Err(ToolError::Other(format!(
                        "在 {target} 中未找到包含 '{old_text}' 的条目"
                    )));
                }
                write_entries(&path, &entries)?;
                Ok(format!("已从 {target} 删除（剩余 {} 条）", entries.len()))
            }
            _ => Err(ToolError::InvalidArg {
                arg: "action",
                reason: format!("未知操作: {action}"),
            }),
        }
    }
}

/// 读取文件并按分隔符拆分为条目。
fn read_entries(path: &PathBuf) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.split(DELIMITER)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 将条目写回文件（覆盖）。
fn write_entries(path: &PathBuf, entries: &[String]) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content: String = entries.iter().map(|e| format!("{DELIMITER}{e}\n")).collect();
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boxing-mem-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn add_and_read_entries() {
        let dir = tmp_dir("add");
        let tool = Memory::new(&dir);
        tool.exec(json!({"action":"add","target":"memory","content":"用户偏好中文注释"}))
            .await
            .unwrap();
        tool.exec(json!({"action":"add","target":"memory","content":"项目使用 boxingAgent"}))
            .await
            .unwrap();
        let text = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(text.contains("中文注释"));
        assert!(text.contains("boxingAgent"));
    }

    #[tokio::test]
    async fn replace_by_old_text() {
        let dir = tmp_dir("replace");
        let tool = Memory::new(&dir);
        tool.exec(json!({"action":"add","target":"memory","content":"旧内容"}))
            .await
            .unwrap();
        tool.exec(json!({"action":"replace","target":"memory","old_text":"旧内容","content":"新内容"}))
            .await
            .unwrap();
        let text = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(text.contains("新内容"));
        assert!(!text.contains("旧内容"));
    }

    #[tokio::test]
    async fn remove_entry() {
        let dir = tmp_dir("remove");
        let tool = Memory::new(&dir);
        tool.exec(json!({"action":"add","target":"memory","content":"要删除的"}))
            .await
            .unwrap();
        tool.exec(json!({"action":"remove","target":"memory","old_text":"要删除的"}))
            .await
            .unwrap();
        let text = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(text.trim().is_empty());
    }
}
