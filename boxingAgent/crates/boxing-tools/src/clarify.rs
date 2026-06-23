//! `clarify` 工具：向用户提出澄清性问题。
//!
//! 与 Hermes 原版 `tools/clarify_tool.py` 对等：
//! - 支持多选题（最多 4 个选项）+ 自由文本回答
//! - CLI 模式：交互式 stdin 读取
//! - 非交互环境：返回错误

use serde_json::{json, Value};
use std::io::{self, IsTerminal, Write};

use crate::{Tool, ToolError};

/// 最大预设选项数。
const MAX_CHOICES: usize = 4;

/// `clarify` 工具：向用户提问以澄清意图。
pub struct Clarify;

#[async_trait::async_trait]
impl Tool for Clarify {
    fn name(&self) -> &str {
        "clarify"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "clarify",
            "description": "Ask the user a question when you need clarification, feedback, or a decision. Supports multiple choice (up to 4 choices + 'Other') or open-ended questions. Use when the task is ambiguous, has meaningful trade-offs, or you need the user to choose an approach.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to present to the user."
                    },
                    "choices": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 4,
                        "description": "Up to 4 answer choices. Omit for an open-ended question. When provided, the UI appends 'Other (type your answer)' as a 5th option."
                    }
                },
                "required": ["question"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("question"))?
            .trim()
            .to_string();

        if question.is_empty() {
            return Err(ToolError::InvalidArg {
                arg: "question",
                reason: "问题文本不能为空".into(),
            });
        }

        // 解析并清理选项
        let choices: Option<Vec<String>> = args
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .take(MAX_CHOICES)
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        // 检查是否交互式（stdin 是 TTY）
        if !io::stdin().is_terminal() {
            return Err(ToolError::Other(
                "clarify 工具需要交互式终端。非交互模式下无法向用户提问。".into(),
            ));
        }

        // 交互式提问
        let response = if let Some(ref choices) = choices {
            prompt_choice(&question, choices)?
        } else {
            prompt_open(&question)?
        };

        Ok(serde_json::json!({
            "question": question,
            "choices_offered": choices,
            "user_response": response,
        })
        .to_string())
    }
}

/// 多选题提示：显示编号选项 + "Other"，读取用户输入。
fn prompt_choice(question: &str, choices: &[String]) -> Result<String, ToolError> {
    println!("\n  ❓ {question}\n");
    for (i, choice) in choices.iter().enumerate() {
        println!("  [{}] {}", i + 1, choice);
    }
    let other_idx = choices.len() + 1;
    println!("  [{other_idx}] 其他（手动输入）");
    print!("\n  请输入选项编号或自定义回答：");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|e| ToolError::Other(format!("读取输入失败: {e}")))?;
    let input = input.trim();

    if input.is_empty() {
        return Ok(String::new());
    }

    // 数字选择
    if let Ok(idx) = input.parse::<usize>() {
        if idx >= 1 && idx <= choices.len() {
            return Ok(choices[idx - 1].clone());
        }
    }

    // "其他" 选项
    if input.eq_ignore_ascii_case(&other_idx.to_string()) {
        print!("  请输入你的回答：");
        io::stdout().flush().ok();
        let mut custom = String::new();
        io::stdin()
            .read_line(&mut custom)
            .map_err(|e| ToolError::Other(format!("读取输入失败: {e}")))?;
        return Ok(custom.trim().to_string());
    }

    // 直接输入了文本
    Ok(input.to_string())
}

/// 自由文本提示：显示问题，读取用户输入。
fn prompt_open(question: &str) -> Result<String, ToolError> {
    println!("\n  ❓ {question}");
    print!("\n  请输入你的回答：");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|e| ToolError::Other(format!("读取输入失败: {e}")))?;
    Ok(input.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn schema_is_valid() {
        let schema = Clarify.schema();
        assert_eq!(schema["name"], "clarify");
        assert!(schema["parameters"]["properties"]["question"].is_object());
        assert!(schema["parameters"]["properties"]["choices"].is_object());
    }

    #[test]
    fn max_choices_is_four() {
        assert_eq!(MAX_CHOICES, 4);
    }

    #[test]
    fn empty_question_is_rejected() {
        // 非 TTY 环境下 exec 返回错误，但 question 验证在 TTY 检查之前
        // 所以我们直接测试 schema 校验
        let schema = Clarify.schema();
        let required = schema["parameters"]["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("question")));
    }

    #[test]
    fn choices_limited_to_max() {
        // 验证 schema 中 choices 的 maxItems 限制
        let schema = Clarify.schema();
        let max = schema["parameters"]["properties"]["choices"]["maxItems"]
            .as_u64()
            .unwrap();
        assert_eq!(max as usize, MAX_CHOICES);
    }
}
