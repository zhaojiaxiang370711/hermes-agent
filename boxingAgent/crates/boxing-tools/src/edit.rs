//! `edit` 工具：唯一匹配的搜索替换。
//!
//! 在文件中查找 `old_string`，要求恰好匹配一次（0 次或多次都报错），
//! 替换为 `new_string` 后写回。

use serde_json::{json, Value};

use crate::{arg_str, Tool, ToolError};

pub struct Edit;

#[async_trait::async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "edit",
            "description": "在文件中用 new_string 替换 old_string 的唯一匹配（0 次或多次匹配均报错）。",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let path = arg_str(&args, "path")?;
        let old_string = arg_str(&args, "old_string")?;
        let new_string = arg_str(&args, "new_string")?;

        let src = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Other(format!("读取 {path} 失败: {e}")))?;
        match src.matches(&old_string).count() {
            0 => Err(ToolError::Other(format!("old_string 在 {path} 中未找到"))),
            1 => {
                let updated = src.replacen(&old_string, &new_string, 1);
                std::fs::write(&path, updated)?;
                Ok(format!("已编辑 {path}"))
            }
            n => Err(ToolError::Other(format!(
                "old_string 在 {path} 中匹配 {n} 次，必须唯一"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str, body: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "boxing-edit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let p = tmp("ok", "foo bar baz");
        let out = Edit
            .exec(json!({"path": p, "old_string": "bar", "new_string": "QUX"}))
            .await
            .unwrap();
        assert!(out.contains("已编辑"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "foo QUX baz");
    }

    #[tokio::test]
    async fn errors_when_not_found_or_not_unique() {
        let p = tmp("miss", "foo bar baz");
        assert!(Edit
            .exec(json!({"path": p, "old_string": "zzz", "new_string": "x"}))
            .await
            .is_err());
        let p2 = tmp("dup", "x x x");
        let err = Edit
            .exec(json!({"path": p2, "old_string": "x", "new_string": "y"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Other(_)));
    }
}
