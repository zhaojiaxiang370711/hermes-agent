//! `session_search` 工具：查询 state.db 中的会话和消息。
//!
//! Schema 与 Hermes `session_search_tool.py` 一致。
//! Phase 1 支持 Discovery（LIKE 搜索）和 Browse（最近会话），Scroll 推迟。
//! 使用 rusqlite 直接查询（不依赖 boxing-state SessionStore）。

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::{Tool, ToolError};

/// `session_search` 工具。
pub struct SessionSearch {
    db_path: PathBuf,
}

impl SessionSearch {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

#[async_trait::async_trait]
impl Tool for SessionSearch {
    fn name(&self) -> &'static str {
        "session_search"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "session_search",
            "description": "Search past sessions and messages. Discovery (with query): keyword search. Browse (no query): recent sessions. Scroll (session_id + around_message_id): not yet supported.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search keywords. Omit to browse recent sessions."},
                    "limit": {"type": "integer", "default": 3, "description": "Max sessions to return (1-10)."},
                    "session_id": {"type": "string", "description": "Session to scroll into (scroll mode)."},
                    "around_message_id": {"type": "integer", "description": "Message id anchor (scroll mode)."},
                    "sort": {"type": "string", "enum": ["newest","oldest"], "description": "Temporal bias for discovery."}
                },
                "required": []
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .min(10) as usize;
        let session_id = args.get("session_id").and_then(|v| v.as_str());
        let around = args.get("around_message_id").and_then(|v| v.as_u64());

        // Scroll 模式：推迟
        if session_id.is_some() && around.is_some() {
            return Ok(
                "scroll 模式尚未支持，请使用 discovery（带 query）或 browse 模式".into(),
            );
        }

        let conn = rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| ToolError::Other(format!("打开 state.db 失败: {e}")))?;

        if !query.is_empty() {
            discovery(&conn, query, limit, args.get("sort").and_then(|v| v.as_str()))
        } else {
            browse(&conn, limit)
        }
    }
}

/// Discovery：按关键词搜索消息，返回匹配的会话。
fn discovery(
    conn: &rusqlite::Connection,
    query: &str,
    limit: usize,
    sort: Option<&str>,
) -> Result<String, ToolError> {
    let order = match sort {
        Some("oldest") => "s.started_at ASC",
        _ => "s.started_at DESC",
    };
    let pattern = format!("%{query}%");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT DISTINCT s.id, s.model, s.title, s.started_at, \
                    m.content, m.role \
             FROM messages m \
             JOIN sessions s ON m.session_id = s.id \
             WHERE m.content LIKE ?1 \
             ORDER BY {order} \
             LIMIT ?2"
        ))
        .map_err(|e| ToolError::Other(e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params![pattern, limit as i64], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| ToolError::Other(e.to_string()))?;

    let mut out = String::new();
    let mut count = 0;
    for row in rows.flatten() {
        let (id, model, title, _started, content, role) = row;
        let snippet: String = content.chars().take(100).collect();
        out.push_str(&format!(
            "session={id} model={model} title={title:?} role={role}\n  {snippet}\n\n"
        ));
        count += 1;
    }
    if count == 0 {
        out.push_str("未找到匹配的会话");
    }
    Ok(out)
}

/// Browse：最近会话。
fn browse(conn: &rusqlite::Connection, limit: usize) -> Result<String, ToolError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, model, title, started_at, message_count \
             FROM sessions ORDER BY started_at DESC LIMIT ?1",
        )
        .map_err(|e| ToolError::Other(e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })
        .map_err(|e| ToolError::Other(e.to_string()))?;

    let mut out = String::new();
    for row in rows.flatten() {
        let (id, model, title, _started, msg_count) = row;
        out.push_str(&format!(
            "session={id} model={model} title={title:?} messages={msg_count}\n"
        ));
    }
    if out.is_empty() {
        out.push_str("没有会话记录");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_db_with_data() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boxing-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (\
               id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, system_prompt TEXT,\
               started_at REAL NOT NULL, message_count INTEGER DEFAULT 0,\
               tool_call_count INTEGER DEFAULT 0, title TEXT);\
             CREATE TABLE messages (\
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,\
               role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT,\
               tool_name TEXT, timestamp REAL NOT NULL, token_count INTEGER,\
               finish_reason TEXT, observed INTEGER DEFAULT 0,\
               active INTEGER NOT NULL DEFAULT 1);",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, source, model, started_at, message_count, title) VALUES \
               ('s1', 'cli', 'mimo', 1000.0, 2, 'fix bug'),\
               ('s2', 'cli', 'mimo', 2000.0, 3, 'add feature');\
             INSERT INTO messages (session_id, role, content, timestamp) VALUES \
               ('s1', 'user', 'fix the login bug', 1001.0),\
               ('s1', 'assistant', 'done', 1002.0),\
               ('s2', 'user', 'add dark mode', 2001.0),\
               ('s2', 'assistant', 'done', 2002.0);",
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn discovery_finds_matching_sessions() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool.exec(json!({"query": "bug"})).await.unwrap();
        assert!(out.contains("fix bug"));
        assert!(!out.contains("dark mode"));
    }

    #[tokio::test]
    async fn browse_returns_recent_sessions() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool.exec(json!({"limit": 1})).await.unwrap();
        assert!(out.contains("s2"));
        assert!(!out.contains("s1"));
    }

    #[tokio::test]
    async fn scroll_not_yet_supported() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool
            .exec(json!({"session_id": "s1", "around_message_id": 1}))
            .await
            .unwrap();
        assert!(out.contains("尚未支持"));
    }
}
