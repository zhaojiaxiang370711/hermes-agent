//! Read-only access to the shared ~/.hermes/state.db (Phase 1b: sessions).
//!
//! Opens the same SQLite file the Python original writes; never creates or
//! mutates tables. Phase 2 adds writes when the agent loop needs them.

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

#[derive(Debug)]
pub struct SessionStore {
    conn: rusqlite::Connection,
}

impl SessionStore {
    /// Open the shared state.db read-only. busy_timeout absorbs transient locks
    /// from a concurrently-writing Python process (WAL allows concurrent readers).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening state db {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
}
