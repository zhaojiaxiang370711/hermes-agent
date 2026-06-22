//! 对共享 ~/.hermes/state.db 的读写访问（Phase 1b：读；Phase 2b：写）。
//!
//! 打开 Python 原版写入的同一 SQLite 文件，读写 + WAL；从不建表/改表结构。

use anyhow::Context;
use rusqlite::OpenFlags;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub model: Option<String>,
    pub started_at: Option<f64>,
    pub message_count: Option<i64>,
    pub title: Option<String>,
}

/// 追加消息的最小记录，覆盖 user/assistant/tool 三种消息。
/// `tool_calls` 为预序列化 JSON（调用方负责）。
pub struct MessageRecord<'a> {
    pub session_id: &'a str,
    pub role: &'a str,
    pub content: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub tool_calls: Option<&'a str>,
    pub tool_call_id: Option<&'a str>,
    pub token_count: Option<i64>,
    pub finish_reason: Option<&'a str>,
}

impl<'a> MessageRecord<'a> {
    pub fn new(session_id: &'a str, role: &'a str) -> Self {
        Self {
            session_id,
            role,
            content: None,
            tool_name: None,
            tool_calls: None,
            tool_call_id: None,
            token_count: None,
            finish_reason: None,
        }
    }
}

#[derive(Debug)]
pub struct SessionStore {
    conn: rusqlite::Connection,
}

impl SessionStore {
    /// 打开共享 state.db（读写 + WAL）。busy_timeout 吸收并发写入方的瞬时锁。
    /// 文件不存在则报错（共享 db 已由 Python 工具创建）。
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening state db {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("setting WAL journal mode")?;
        Ok(Self { conn })
    }

    pub fn session_count(&self) -> anyhow::Result<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .context("counting sessions")?;
        Ok(n)
    }

    pub fn session_summaries(&self) -> anyhow::Result<Vec<SessionSummary>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, model, started_at, message_count, title \
                 FROM sessions ORDER BY started_at DESC",
            )
            .context("preparing sessions query")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SessionSummary {
                    id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    model: r.get(1)?,
                    started_at: r.get(2)?,
                    message_count: r.get(3)?,
                    title: r.get(4)?,
                })
            })
            .context("querying sessions")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 创建会话：INSERT sessions(id, source, started_at=now, model?, system_prompt?)。
    /// 重复 id → PRIMARY KEY 约束错误（向上传播）。
    pub fn create_session(
        &self,
        id: &str,
        source: &str,
        model: Option<&str>,
        system_prompt: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        self.conn
            .execute(
                "INSERT INTO sessions (id, source, started_at, model, system_prompt) \
                 VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![id, source, now, model, system_prompt],
            )
            .context("creating session")?;
        Ok(())
    }

    /// 追加消息：INSERT messages(...) + 递增 sessions.message_count（同一事务）。
    /// role=='tool' 或 tool_calls 非空时，同时递增 tool_call_count。返回新消息行 id。
    pub fn append_message(&mut self, rec: &MessageRecord) -> anyhow::Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO messages \
             (session_id, role, content, tool_name, tool_calls, tool_call_id, token_count, finish_reason, timestamp) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                rec.session_id,
                rec.role,
                rec.content,
                rec.tool_name,
                rec.tool_calls,
                rec.tool_call_id,
                rec.token_count,
                rec.finish_reason,
                now
            ],
        )
        .context("inserting message")?;
        let id = tx.last_insert_rowid();
        let bump_tool = rec.role == "tool" || rec.tool_calls.is_some();
        if bump_tool {
            tx.execute(
                "UPDATE sessions SET message_count = message_count + 1, \
                 tool_call_count = tool_call_count + 1 WHERE id = ?",
                rusqlite::params![rec.session_id],
            )?;
        } else {
            tx.execute(
                "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?",
                rusqlite::params![rec.session_id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-state-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture() -> PathBuf {
        let dir = unique_dir("filled");
        let path = dir.join("state.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Minimal sessions table mirroring the columns Phase 1b reads.
        conn.execute_batch(
            "CREATE TABLE sessions (\
               id TEXT PRIMARY KEY, model TEXT, started_at REAL, \
               message_count INTEGER, title TEXT);\
             INSERT INTO sessions (id, model, started_at, message_count, title) VALUES \
               ('s1', 'mimo', 1000.0, 3, 'first'),\
               ('s2', 'mimo', 2000.0, 5, 'second');",
        )
        .unwrap();
        drop(conn);
        path
    }

    #[test]
    fn counts_and_lists_sessions() {
        let path = fixture();
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(store.session_count().unwrap(), 2);
        let sums = store.session_summaries().unwrap();
        assert_eq!(sums.len(), 2);
        assert_eq!(sums[0].id, "s2"); // ORDER BY started_at DESC
        assert_eq!(sums[0].message_count, Some(5));
        assert_eq!(sums[1].id, "s1");
        assert_eq!(sums[1].title.as_deref(), Some("first"));
    }

    #[test]
    fn empty_db_has_zero_sessions() {
        let dir = unique_dir("empty");
        let path = dir.join("empty.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, started_at REAL, message_count INTEGER, title TEXT)",
            [],
        )
        .unwrap();
        drop(conn);
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(store.session_count().unwrap(), 0);
        assert!(store.session_summaries().unwrap().is_empty());
    }

    #[test]
    fn open_is_read_write_and_wal() {
        let path = fixture();
        let store = SessionStore::open(&path).unwrap();
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "应为 WAL 日志模式");
        assert_eq!(store.session_count().unwrap(), 2); // 读仍可用
    }

    /// 建临时库，含 boxing-state 读写的 sessions/messages 列（非完整 30+ 列，
    /// 只覆盖 boxing-state 触碰的列 + NOT NULL/默认列）。
    fn schema_db() -> PathBuf {
        let dir = unique_dir("schema");
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
        drop(conn);
        path
    }

    #[test]
    fn create_session_inserts_row() {
        let path = schema_db();
        let store = SessionStore::open(&path).unwrap();
        store
            .create_session("s1", "cli", Some("mimo"), Some("sys"))
            .unwrap();
        let (source, model, sys): (String, Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT source, model, system_prompt FROM sessions WHERE id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(source, "cli");
        assert_eq!(model.as_deref(), Some("mimo"));
        assert_eq!(sys.as_deref(), Some("sys"));
    }

    #[test]
    fn create_session_duplicate_id_errors() {
        let path = schema_db();
        let store = SessionStore::open(&path).unwrap();
        store.create_session("s1", "cli", None, None).unwrap();
        assert!(store.create_session("s1", "cli", None, None).is_err());
    }

    #[test]
    fn append_message_inserts_and_bumps_count() {
        let path = schema_db();
        let mut store = SessionStore::open(&path).unwrap();
        store.create_session("s1", "cli", None, None).unwrap();

        let mut rec = MessageRecord::new("s1", "user");
        rec.content = Some("hello");
        let id = store.append_message(&rec).unwrap();
        assert!(id > 0);

        let (role, content): (String, Option<String>) = store
            .conn
            .query_row(
                "SELECT role, content FROM messages WHERE id = ?",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(role, "user");
        assert_eq!(content.as_deref(), Some("hello"));

        let (mc, tcc): (i64, i64) = store
            .conn
            .query_row(
                "SELECT message_count, tool_call_count FROM sessions WHERE id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mc, 1);
        assert_eq!(tcc, 0, "user 消息不递增 tool_call_count");
    }

    #[test]
    fn append_tool_message_bumps_tool_call_count() {
        let path = schema_db();
        let mut store = SessionStore::open(&path).unwrap();
        store.create_session("s1", "cli", None, None).unwrap();

        let mut rec = MessageRecord::new("s1", "tool");
        rec.content = Some("result");
        rec.tool_name = Some("bash");
        rec.tool_call_id = Some("call_1");
        store.append_message(&rec).unwrap();

        let (mc, tcc): (i64, i64) = store
            .conn
            .query_row(
                "SELECT message_count, tool_call_count FROM sessions WHERE id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mc, 1);
        assert_eq!(tcc, 1, "tool 消息递增 tool_call_count");
    }
}
