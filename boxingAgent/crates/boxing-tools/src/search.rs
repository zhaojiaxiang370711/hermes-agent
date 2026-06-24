//! `session_search` 工具：查询 state.db 中的会话和消息。
//!
//! 与 Hermes 原版 `tools/session_search_tool.py` 对等，四种调用形态（按参数推断，无显式 mode）：
//! - **Discovery**（带 `query`）：LIKE 搜索消息内容，返回匹配的会话片段
//! - **Scroll**（`session_id` + `around_message_id`）：锚定消息窗口，**按位置开窗**
//!   （对等 `get_messages_around`：`id<=anchor DESC LIMIT w+1` + `id>anchor ASC LIMIT w`）
//! - **Read**（`session_id` 无 anchor）：导出整会话（head 20 + tail 10）
//! - **Browse**（无参）：最近会话
//!
//! `role_filter` 仅作用于 discovery（对等源码：`_scroll` 完全不应用 role_filter，
//! `get_messages_around` 也不按 active 过滤；read 走 `get_messages` 默认即 active=1）。
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
            "description": "Search past sessions stored in the local session DB, or read/scroll inside one. Discovery (with query): keyword search. Browse (no args): recent sessions. Scroll (session_id + around_message_id): anchored message window. Read (session_id only): dump the whole session. role_filter applies to discovery only (default: user,assistant).",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search keywords (discovery). Omit to browse recent sessions."},
                    "limit": {"type": "integer", "default": 3, "description": "Max sessions to return (discovery, 1-10)."},
                    "session_id": {"type": "string", "description": "Session to scroll into (with around_message_id) or read in full (without around_message_id)."},
                    "around_message_id": {"type": "integer", "description": "Message id anchor (scroll mode)."},
                    "window": {"type": "integer", "default": 5, "description": "Messages on each side of anchor (scroll mode, 1-20)."},
                    "sort": {"type": "string", "enum": ["newest","oldest"], "description": "Temporal bias for discovery."},
                    "role_filter": {"type": "string", "description": "Discovery only. Comma-separated roles to include (default: user,assistant)."}
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
            .clamp(1, 10) as usize;
        // 空串视同未传（对等源码 session_id.strip() 检查）
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let around = args
            .get("around_message_id")
            .and_then(|v| v.as_u64())
            .map(|v| v as i64);
        let window = args
            .get("window")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20) as usize;
        let role_filter = args.get("role_filter").and_then(|v| v.as_str());
        let sort = args.get("sort").and_then(|v| v.as_str());

        let conn = rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| ToolError::Other(format!("打开 state.db 失败: {e}")))?;

        // 派发顺序对等源码：scroll → read → discovery → browse
        if let (Some(sid), Some(anchor)) = (session_id, around) {
            return scroll(&conn, sid, anchor, window);
        }
        if let Some(sid) = session_id {
            return read(&conn, sid);
        }
        if !query.is_empty() {
            discovery(&conn, query, limit, sort, role_filter)
        } else {
            browse(&conn, limit)
        }
    }
}

/// 把一行消息映射为 (id, role, content, timestamp)。
fn map_msg(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, String, f64)> {
    Ok((
        r.get::<_, Option<i64>>(0)?.unwrap_or(0),
        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
    ))
}

/// 格式化一行消息（200 字符片段，可选锚点标记）。
fn fmt_line(id: i64, role: &str, content: &str, marker: &str) -> String {
    let snippet: String = content.chars().take(200).collect();
    format!("[{id}] {role}: {snippet}{marker}\n")
}

/// Discovery：按关键词搜索消息，返回匹配的会话片段。
fn discovery(
    conn: &rusqlite::Connection,
    query: &str,
    limit: usize,
    sort: Option<&str>,
    role_filter: Option<&str>,
) -> Result<String, ToolError> {
    let order = match sort {
        Some("oldest") => "s.started_at ASC",
        _ => "s.started_at DESC",
    };
    let pattern = format!("%{query}%");
    let roles: Vec<&str> = role_filter
        .map(|r| r.split(',').map(str::trim).collect())
        .unwrap_or_else(|| vec!["user", "assistant"]);
    let placeholders: Vec<String> = roles
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 3))
        .collect();
    let role_clause = format!("AND m.role IN ({})", placeholders.join(", "));

    let sql = format!(
        "SELECT DISTINCT s.id, s.model, s.title, s.started_at, \
                m.id, m.content, m.role \
         FROM messages m \
         JOIN sessions s ON m.session_id = s.id \
         WHERE m.content LIKE ?1 \
         {role_clause} \
         ORDER BY {order} \
         LIMIT ?2"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ToolError::Other(e.to_string()))?;

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(pattern), Box::new(limit as i64)];
    for r in &roles {
        params.push(Box::new(r.to_string()));
    }

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| ToolError::Other(e.to_string()))?;

    let mut out = String::new();
    let mut count = 0;
    for row in rows.flatten() {
        let (sid, model, title, _started, msg_id, content, role) = row;
        let snippet: String = content.chars().take(200).collect();
        out.push_str(&format!(
            "session={sid} model={model} title={title:?} role={role} message_id={msg_id}\n  {snippet}\n\n"
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

/// Read：导出整会话（head 20 + tail 10，对等 `_read_session` + `get_messages` 默认 active=1）。
fn read(conn: &rusqlite::Connection, session_id: &str) -> Result<String, ToolError> {
    const HEAD: usize = 20;
    const TAIL: usize = 10;

    // 会话元信息
    let meta = conn.query_row(
        "SELECT model, title, source, started_at FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            ))
        },
    );
    let (model, title, source, _started) = match meta {
        Ok(m) => m,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Ok(format!("session_id not found: {session_id}"))
        }
        Err(e) => return Err(ToolError::Other(e.to_string())),
    };

    // 全部 active 消息，按插入顺序（id ASC）
    let mut msgs: Vec<(i64, String, String, f64)> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, timestamp FROM messages \
                 WHERE session_id = ?1 AND active = 1 ORDER BY id",
            )
            .map_err(|e| ToolError::Other(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![session_id], map_msg)
            .map_err(|e| ToolError::Other(e.to_string()))?;
        for r in rows {
            msgs.push(r.map_err(|e| ToolError::Other(e.to_string()))?);
        }
    }

    let total = msgs.len();
    let truncated = total > HEAD + TAIL;

    let mut out = String::new();
    out.push_str(&format!(
        "session={session_id} mode=read model={model} title={title:?} source={source} messages={total}\n"
    ));

    if truncated {
        for (id, role, content, _ts) in msgs.iter().take(HEAD) {
            out.push_str(&fmt_line(*id, role, content, ""));
        }
        out.push_str(&format!(
            "  --- 中间省略 {} 条；传 around_message_id（上面任一 id）可滚动查看 ---\n",
            total - HEAD - TAIL
        ));
        for (id, role, content, _ts) in msgs.iter().skip(total - TAIL) {
            out.push_str(&fmt_line(*id, role, content, ""));
        }
    } else {
        for (id, role, content, _ts) in msgs.iter() {
            out.push_str(&fmt_line(*id, role, content, ""));
        }
    }
    Ok(out)
}

/// Scroll：锚定消息窗口（对等 `get_messages_around`）。
///
/// 按位置开窗：`id<=anchor DESC LIMIT window+1`（含锚点）+ `id>anchor ASC LIMIT window`，
/// 合并后按 id ASC。message id 全局 AUTOINCREMENT、会话内不连续，故必须按位置而非 id 范围开窗。
/// 不过滤 active，不应用 role_filter（与源码一致）。
fn scroll(
    conn: &rusqlite::Connection,
    session_id: &str,
    anchor_id: i64,
    window: usize,
) -> Result<String, ToolError> {
    // 锚点存在性
    let anchor_ok = match conn.query_row(
        "SELECT 1 FROM messages WHERE id = ?1 AND session_id = ?2 LIMIT 1",
        rusqlite::params![anchor_id, session_id],
        |_| Ok(()),
    ) {
        Ok(()) => true,
        Err(rusqlite::Error::QueryReturnedNoRows) => false,
        Err(e) => return Err(ToolError::Other(e.to_string())),
    };
    if !anchor_ok {
        return Ok(format!("around_message_id {anchor_id} not in session_id {session_id}"));
    }

    // before：锚点 + 最多 window 条之前的消息（DESC，取 window+1）
    let mut before: Vec<(i64, String, String, f64)> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, timestamp FROM messages \
                 WHERE session_id = ?1 AND id <= ?2 \
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(|e| ToolError::Other(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![session_id, anchor_id, (window as i64) + 1], map_msg)
            .map_err(|e| ToolError::Other(e.to_string()))?;
        for r in rows {
            before.push(r.map_err(|e| ToolError::Other(e.to_string()))?);
        }
    }
    before.reverse(); // 恢复 id ASC

    // after：锚点之后最多 window 条（ASC）
    let mut after: Vec<(i64, String, String, f64)> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, timestamp FROM messages \
                 WHERE session_id = ?1 AND id > ?2 \
                 ORDER BY id ASC LIMIT ?3",
            )
            .map_err(|e| ToolError::Other(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![session_id, anchor_id, window as i64], map_msg)
            .map_err(|e| ToolError::Other(e.to_string()))?;
        for r in rows {
            after.push(r.map_err(|e| ToolError::Other(e.to_string()))?);
        }
    }

    let messages_before = before.len().saturating_sub(1); // before 含锚点
    let messages_after = after.len();

    let mut out = String::new();
    out.push_str(&format!(
        "session={session_id} mode=scroll around={anchor_id} window={window} before={messages_before} after={messages_after}\n"
    ));
    for (id, role, content, _ts) in before.iter().chain(after.iter()) {
        let marker = if *id == anchor_id { " >>" } else { "" };
        out.push_str(&fmt_line(*id, role, content, marker));
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

    /// 非连续 id 的会话：s1 的消息 id 为 10/20/30（中间隔了 s2 的消息），
    /// 用于验证 scroll 必须按位置开窗、而非 id 范围。
    fn db_with_gaps() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boxing-search-gap-{}-{}",
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
               active INTEGER NOT NULL DEFAULT 1);\
             INSERT INTO sessions (id, source, model, started_at, title) VALUES \
               ('g1', 'cli', 'mimo', 1000.0, 'gap session');\
             INSERT INTO messages (id, session_id, role, content, timestamp) VALUES \
               (10, 'g1', 'user', 'msg ten', 1000.0),\
               (20, 'g1', 'assistant', 'msg twenty', 1001.0),\
               (30, 'g1', 'user', 'msg thirty', 1002.0);",
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
    async fn scroll_returns_anchored_messages() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool
            .exec(json!({"session_id": "s1", "around_message_id": 1, "window": 10}))
            .await
            .unwrap();
        assert!(out.contains("mode=scroll"));
        assert!(out.contains("fix the login bug"));
        assert!(out.contains("done"));
        assert!(out.contains(">>")); // 锚点标记
    }

    #[tokio::test]
    async fn scroll_positional_window_with_non_contiguous_ids() {
        // s1 消息 id=10/20/30；anchor=20, window=1 → 应返回 [10, 20, 30]
        // （id 范围 BETWEEN 19..21 只会命中 20，验证按位置开窗的正确性）
        let db = db_with_gaps();
        let tool = SessionSearch::new(db);
        let out = tool
            .exec(json!({"session_id": "g1", "around_message_id": 20, "window": 1}))
            .await
            .unwrap();
        assert!(out.contains("msg ten"));
        assert!(out.contains("msg twenty"));
        assert!(out.contains("msg thirty"));
    }

    #[tokio::test]
    async fn scroll_window_clamps_lower_bound() {
        // window=0 钳制为 1：anchor=20, window=1 → 仍含相邻的 10 与 30
        let db = db_with_gaps();
        let tool = SessionSearch::new(db);
        let out = tool
            .exec(json!({"session_id": "g1", "around_message_id": 20, "window": 0}))
            .await
            .unwrap();
        assert!(out.contains("msg twenty"));
        assert!(out.contains("window=1"));
    }

    #[tokio::test]
    async fn scroll_anchor_not_found() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool
            .exec(json!({"session_id": "s1", "around_message_id": 9999, "window": 5}))
            .await
            .unwrap();
        assert!(out.contains("not in session_id"));
    }

    #[tokio::test]
    async fn role_filter_limits_discovery_only() {
        // "done" 只出现在 assistant 消息里，用于真正验证 role_filter
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let user_only = tool
            .exec(json!({"query": "done", "role_filter": "user"}))
            .await
            .unwrap();
        assert!(user_only.contains("未找到")); // user 角色里没有 "done"
        let with_assistant = tool.exec(json!({"query": "done"})).await.unwrap();
        assert!(with_assistant.contains("role=assistant"));
    }

    #[tokio::test]
    async fn scroll_ignores_role_filter() {
        // scroll 不应用 role_filter：assistant 仍应出现（对等源码 _scroll）
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool
            .exec(json!({"session_id": "s1", "around_message_id": 1, "window": 10, "role_filter": "user"}))
            .await
            .unwrap();
        assert!(out.contains("fix the login bug"));
        assert!(out.contains("done")); // role_filter 在 scroll 被忽略
    }

    #[tokio::test]
    async fn read_dumps_whole_session() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool.exec(json!({"session_id": "s1"})).await.unwrap();
        assert!(out.contains("mode=read"));
        assert!(out.contains("fix the login bug"));
        assert!(out.contains("done"));
        assert!(out.contains("messages=2"));
    }

    #[tokio::test]
    async fn read_not_found() {
        let db = schema_db_with_data();
        let tool = SessionSearch::new(db);
        let out = tool.exec(json!({"session_id": "nope"})).await.unwrap();
        assert!(out.contains("not found"));
    }

    #[tokio::test]
    async fn read_truncates_large_session() {
        // 35 条消息 → head 20 + tail 10，中间 5 条被省略
        let dir = std::env::temp_dir().join(format!(
            "boxing-read-trunc-{}-{}",
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
               active INTEGER NOT NULL DEFAULT 1);\
             INSERT INTO sessions (id, source, model, started_at, title) VALUES \
               ('big', 'cli', 'mimo', 1000.0, 'big session');",
        )
        .unwrap();
        for i in 1..=35 {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('big', 'user', ?1, ?2)",
                rusqlite::params![format!("content {i}"), i as f64],
            )
            .unwrap();
        }
        drop(conn);

        let tool = SessionSearch::new(path);
        let out = tool.exec(json!({"session_id": "big"})).await.unwrap();
        assert!(out.contains("messages=35"));
        assert!(out.contains("content 1")); // head
        assert!(out.contains("content 35")); // tail
        assert!(out.contains("content 20")); // head 末
        assert!(!out.contains("content 25")); // 中间被省略
        assert!(out.contains("省略")); // 截断提示
    }
}
