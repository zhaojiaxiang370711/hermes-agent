//! 记忆自动注入模块。
//!
//! 从 `~/.hermes/MEMORY.md` 和 `~/.hermes/USER.md` 读取条目，
//! 格式化为系统提示的"冻结快照"，在每次 run() 时自动注入。
//! 采用 Hermes 原版的快照模式：会话开始时读取一次，之后不再变化。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 条目分隔符（与 Hermes 原版一致）。
const ENTRY_DELIMITER: &str = "§";

/// 单个记忆/用户画像条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub content: String,
}

/// 记忆注入器：管理 MEMORY.md 和 USER.md 的快照。
pub struct MemoryInjector {
    /// 记忆快照（会话开始时加载）
    memory_snapshot: Option<String>,
    /// 用户画像快照
    user_snapshot: Option<String>,
}

impl MemoryInjector {
    /// 从指定路径加载记忆文件，生成快照。
    pub fn load(hermes_home: &Path) -> Self {
        let memory_path = hermes_home.join("MEMORY.md");
        let user_path = hermes_home.join("USER.md");

        let memory_snapshot = Self::load_and_format(&memory_path, "memory");
        let user_snapshot = Self::load_and_format(&user_path, "user");

        Self {
            memory_snapshot,
            user_snapshot,
        }
    }

    /// 加载文件并格式化为系统提示块。
    fn load_and_format(path: &Path, target: &str) -> Option<String> {
        if !path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(path).ok()?;
        let entries: Vec<&str> = content
            .split(ENTRY_DELIMITER)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if entries.is_empty() {
            return None;
        }

        let joined = entries.join("\n");
        let header = if target == "user" {
            "USER PROFILE (who the user is)"
        } else {
            "MEMORY (your personal notes)"
        };

        let separator = "═".repeat(46);
        Some(format!("{separator}\n{header}\n{separator}\n{joined}"))
    }

    /// 获取记忆快照（用于系统提示注入）。
    pub fn memory_snapshot(&self) -> Option<&str> {
        self.memory_snapshot.as_deref()
    }

    /// 获取用户画像快照。
    pub fn user_snapshot(&self) -> Option<&str> {
        self.user_snapshot.as_deref()
    }

    /// 组装完整的记忆块（记忆 + 用户画像）。
    pub fn build_memory_block(&self) -> Option<String> {
        let mut parts = Vec::new();

        if let Some(mem) = &self.memory_snapshot {
            parts.push(mem.clone());
        }
        if let Some(user) = &self.user_snapshot {
            parts.push(user.clone());
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_hermes_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boxing-mem-inject-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_memory_and_user_files() {
        let dir = temp_hermes_home();
        fs::write(
            dir.join("MEMORY.md"),
            "§记住了中文注释规范\n§项目使用Rust重写",
        )
        .unwrap();
        fs::write(dir.join("USER.md"), "§喜欢简洁的代码风格").unwrap();

        let injector = MemoryInjector::load(&dir);

        assert!(injector.memory_snapshot().is_some());
        assert!(injector.user_snapshot().is_some());
        assert!(injector.build_memory_block().is_some());

        let block = injector.build_memory_block().unwrap();
        assert!(block.contains("记住了中文注释规范"));
        assert!(block.contains("喜欢简洁的代码风格"));
    }

    #[test]
    fn handles_missing_files() {
        let dir = temp_hermes_home();
        // 不创建任何文件
        let injector = MemoryInjector::load(&dir);

        assert!(injector.memory_snapshot().is_none());
        assert!(injector.user_snapshot().is_none());
        assert!(injector.build_memory_block().is_none());
    }

    #[test]
    fn handles_empty_files() {
        let dir = temp_hermes_home();
        fs::write(dir.join("MEMORY.md"), "").unwrap();
        fs::write(dir.join("USER.md"), "").unwrap();

        let injector = MemoryInjector::load(&dir);

        assert!(injector.memory_snapshot().is_none());
        assert!(injector.user_snapshot().is_none());
    }
}
