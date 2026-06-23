//! `todo` 工具：内存任务列表。
//!
//! Schema 与 Hermes 原版 `todo_tool.py` 一致。省略 `todos` 则读取当前列表。
//! `merge=true` 按 id 更新 + 追加新项；`merge=false`（默认）替换整个列表。
//! 始终返回当前完整列表（JSON 字符串）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;

use crate::{Tool, ToolError};

/// 单条任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String, // pending | in_progress | completed | cancelled
}

/// `todo` 工具：持有内存中的任务列表。
pub struct Todo {
    store: Mutex<Vec<TodoItem>>,
}

impl Todo {
    pub fn new() -> Self {
        Self {
            store: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for Todo {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "todo",
            "description": "Manage a task list. Omit 'todos' to read current list. Set 'merge'=true to update by id + append new items; default (false) replaces the entire list. Always returns the full current list.",
            "parameters": {
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "Task items to write. Omit to read current list.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending","in_progress","completed","cancelled"]}
                            },
                            "required": ["id", "content", "status"]
                        }
                    },
                    "merge": {
                        "type": "boolean",
                        "description": "true: update existing by id + append new. false: replace entire list.",
                        "default": false
                    }
                },
                "required": []
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let mut store = self
            .store
            .lock()
            .map_err(|e| ToolError::Other(e.to_string()))?;

        // 读取模式：省略 todos 则返回当前列表
        let todos_val = match args.get("todos") {
            Some(v) => v,
            None => return Ok(serde_json::to_string(&*store).unwrap_or_default()),
        };

        let todos: Vec<TodoItem> =
            serde_json::from_value(todos_val.clone()).map_err(|e| ToolError::InvalidArg {
                arg: "todos",
                reason: e.to_string(),
            })?;
        let merge = args.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);

        if merge {
            // 按 id 更新已有项，追加新项
            let mut indexed: std::collections::HashMap<String, TodoItem> =
                store.drain(..).map(|t| (t.id.clone(), t)).collect();
            for item in todos {
                indexed.insert(item.id.clone(), item);
            }
            *store = indexed.into_values().collect();
        } else {
            // 替换整个列表
            *store = todos;
        }

        Ok(serde_json::to_string(&*store).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_empty_list() {
        let todo = Todo::new();
        let out = todo.exec(json!({})).await.unwrap();
        assert_eq!(out, "[]");
    }

    #[tokio::test]
    async fn write_and_read() {
        let todo = Todo::new();
        let items = json!({"todos": [
            {"id": "1", "content": "fix bug", "status": "pending"},
            {"id": "2", "content": "write tests", "status": "in_progress"}
        ]});
        let out = todo.exec(items).await.unwrap();
        assert!(out.contains("fix bug"));
        assert!(out.contains("write tests"));

        // 读取（省略 todos）
        let read = todo.exec(json!({})).await.unwrap();
        assert_eq!(read, out);
    }

    #[tokio::test]
    async fn merge_updates_by_id() {
        let todo = Todo::new();
        todo.exec(json!({"todos": [
            {"id": "1", "content": "a", "status": "pending"},
            {"id": "2", "content": "b", "status": "pending"}
        ]}))
        .await
        .unwrap();

        // merge=true：更新 id=1 状态，追加 id=3
        let out = todo
            .exec(json!({
                "todos": [{"id": "1", "content": "a", "status": "completed"},
                          {"id": "3", "content": "c", "status": "pending"}],
                "merge": true
            }))
            .await
            .unwrap();
        assert!(out.contains("completed"));
        assert!(out.contains("c"));
        assert!(out.contains("b")); // id=2 保留
    }
}
