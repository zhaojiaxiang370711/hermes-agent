//! `grep` 工具：gitignore 感知的递归正则内容搜索。
//!
//! 三种输出模式：`content`（path:line:match）、`files_only`（匹配文件路径）、
//! `count`（path:count）。`content` 模式可用 `context` 带上下文行。

use regex::Regex;
use serde_json::{json, Value};

use crate::{arg_optional_str, arg_optional_u64, arg_str, Tool, ToolError};

/// 单次最多输出的 match 行数（content 模式）。
const MAX_MATCHES: usize = 500;

pub struct Grep;

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "grep",
            "description": "递归搜索文件内容（正则，gitignore 感知）。output_mode: content|files_only|count。",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "正则表达式"},
                    "path": {"type": "string", "default": "."},
                    "include": {"type": "string", "description": "文件名 glob 过滤，如 *.rs"},
                    "output_mode": {"type": "string", "enum": ["content", "files_only", "count"], "default": "content"},
                    "context": {"type": "integer", "default": 0, "description": "content 模式下前后上下文行数"}
                },
                "required": ["pattern"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let pattern = arg_str(&args, "pattern")?;
        let path = arg_optional_str(&args, "path").unwrap_or_else(|| ".".into());
        let include = arg_optional_str(&args, "include");
        let mode = arg_optional_str(&args, "output_mode").unwrap_or_else(|| "content".into());
        let context = arg_optional_u64(&args, "context").unwrap_or(0) as usize;

        let re = Regex::new(&pattern)?; // 校验正则
        let include_re = include.as_deref().and_then(|g| glob::Pattern::new(g).ok());

        // ignore 默认跳过隐藏文件与 gitignored 文件（与 ripgrep 一致）。
        let walker = ignore::WalkBuilder::new(&path).build();
        let mut out: Vec<String> = Vec::new();
        let mut shown = 0usize;

        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let p = entry.path();
            if let Some(g) = &include_re {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !g.matches(&name) {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(p) else {
                continue;
            };
            let rel = p.to_string_lossy().into_owned();
            let lines: Vec<&str> = text.lines().collect();
            let mut file_count = 0u64;

            for (i, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    file_count += 1;
                    if mode == "content" && shown < MAX_MATCHES {
                        if context == 0 {
                            out.push(format!("{rel}:{}:{}", i + 1, line));
                        } else {
                            // 带上下文：输出 [i-context, i+context] 区间
                            let lo = i.saturating_sub(context);
                            let hi = (i + context + 1).min(lines.len());
                            for (offset, line) in lines[lo..hi].iter().enumerate() {
                                out.push(format!("{rel}:{}:{}", lo + offset + 1, line));
                            }
                        }
                        shown += 1;
                    }
                }
            }
            if file_count > 0 {
                if mode == "files_only" {
                    out.push(rel);
                } else if mode == "count" {
                    out.push(format!("{rel}:{file_count}"));
                }
            }
        }
        if mode == "content" && shown >= MAX_MATCHES {
            out.push(format!("<已截断，仅显示前 {MAX_MATCHES} 条匹配>"));
        }
        Ok(out.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> String {
        let dir = std::env::temp_dir().join(format!(
            "boxing-grep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        std::fs::write(dir.join("b.txt"), "fn ignored\n").unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn content_mode_respects_include() {
        let dir = fixture();
        let out = Grep
            .exec(
                json!({"pattern": "fn ", "path": dir, "include": "*.rs", "output_mode": "content"}),
            )
            .await
            .unwrap();
        assert!(out.contains("a.rs:1:fn main"));
        assert!(out.contains("a.rs:2:fn helper"));
        assert!(!out.contains("b.txt")); // include 过滤掉
    }

    #[tokio::test]
    async fn files_only_and_count_modes() {
        let dir = fixture();
        let files = Grep
            .exec(json!({"pattern": "fn ", "path": dir, "include": "*.rs", "output_mode": "files_only"}))
            .await
            .unwrap();
        assert!(files.contains("a.rs") && !files.contains(':'));
        let cnt = Grep
            .exec(json!({"pattern": "fn ", "path": dir, "include": "*.rs", "output_mode": "count"}))
            .await
            .unwrap();
        assert!(cnt.contains("a.rs:2"));
    }

    #[tokio::test]
    async fn bad_regex_errors() {
        let dir = fixture();
        assert!(Grep
            .exec(json!({"pattern": "*invalid", "path": dir}))
            .await
            .is_err());
    }
}
