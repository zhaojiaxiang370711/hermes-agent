//! `web_search` 工具：联网搜索。
//!
//! 与 Hermes 原版 `tools/web_tools.py` 的 web_search 对等：
//! - 支持 DuckDuckGo（免费，无需 API key）
//! - 返回结构化结果（title, url, description）
//! - 后续可扩展 SearXNG / Bing 等后端
//!
//! 当前实现：DuckDuckGo HTML 搜索（简化版）

use reqwest;
use serde_json::{json, Value};

use crate::{Tool, ToolError};

/// DuckDuckGo 搜索端点
const DDG_SEARCH_URL: &str = "https://html.duckduckgo.com/html/";

/// `web_search` 工具：联网搜索信息。
pub struct WebSearch;

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "web_search",
            "description": "Search the web for information. Returns up to 5 results by default with titles, URLs, and descriptions. Uses DuckDuckGo (free, no API key required).",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query to look up on the web."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (1-10, default 5).",
                        "minimum": 1,
                        "maximum": 10,
                        "default": 5
                    }
                },
                "required": ["query"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("query"))?;

        if query.trim().is_empty() {
            return Err(ToolError::InvalidArg {
                arg: "query",
                reason: "搜索词不能为空".into(),
            });
        }

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 10) as usize;

        let results = ddg_search(query, limit).await?;

        Ok(serde_json::to_string(&results).unwrap_or_else(|_| "{}".to_string()))
    }
}

/// DuckDuckGo 搜索结果
struct SearchResult {
    title: String,
    url: String,
    description: String,
}

/// 执行 DuckDuckGo HTML 搜索。
async fn ddg_search(query: &str, limit: usize) -> Result<Value, ToolError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (compatible; boxingAgent/0.1)")
        .build()
        .map_err(|e| ToolError::Other(format!("创建 HTTP 客户端: {e}")))?;

    // 发送 POST 请求到 DuckDuckGo HTML 搜索
    let response = client
        .post(DDG_SEARCH_URL)
        .form(&[("q", query)])
        .send()
        .await
        .map_err(|e| ToolError::Other(format!("搜索请求失败: {e}")))?;

    let html = response
        .text()
        .await
        .map_err(|e| ToolError::Other(format!("读取响应失败: {e}")))?;

    // 解析 HTML 结果（简化版：提取链接和文本）
    let results = parse_ddg_html(&html, limit);

    let web_results: Vec<Value> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "title": r.title,
                "url": r.url,
                "description": r.description,
                "position": i + 1
            })
        })
        .collect();

    Ok(json!({
        "success": true,
        "data": { "web": web_results }
    }))
}

/// 从 DuckDuckGo HTML 中解析搜索结果（简化版 HTML 解析）。
fn parse_ddg_html(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // DuckDuckGo HTML 结果格式：
    // <a class="result__a" href="URL">TITLE</a>
    // <a class="result__snippet">DESCRIPTION</a>

    for (i, line) in html.lines().enumerate() {
        if results.len() >= limit {
            break;
        }

        // 查找结果链接
        if line.contains("result__a") && line.contains("href=") {
            let url = extract_attr(line, "href").unwrap_or_default();
            let title = extract_between(line, ">", "</a>").unwrap_or_default();

            // 下一行通常是描述
            let desc = html
                .lines()
                .nth(i + 1)
                .and_then(|l| {
                    if l.contains("result__snippet") {
                        extract_between(l, ">", "</a>")
                            .or_else(|| extract_between(l, ">", "</span>"))
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            if !url.is_empty() && !title.is_empty() {
                results.push(SearchResult {
                    title: clean_html(&title),
                    url,
                    description: clean_html(&desc),
                });
            }
        }
    }

    results
}

/// 从 HTML 标签中提取属性值。
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// 从 HTML 中提取标签之间的文本。
fn extract_between(html: &str, start_tag: &str, end_tag: &str) -> Option<String> {
    let start_idx = html.find(start_tag)? + start_tag.len();
    let end_idx = html[start_idx..].find(end_tag)? + start_idx;
    Some(html[start_idx..end_idx].to_string())
}

/// 清理 HTML 标签（移除标签，保留文本）。
fn clean_html(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid() {
        let schema = WebSearch.schema();
        assert_eq!(schema["name"], "web_search");
        assert!(schema["parameters"]["properties"]["query"].is_object());
        assert!(schema["parameters"]["properties"]["limit"].is_object());
    }

    #[test]
    fn clean_html_works() {
        assert_eq!(clean_html("hello <b>world</b>"), "hello world");
        assert_eq!(clean_html("plain text"), "plain text");
        assert_eq!(clean_html("<a href=\"url\">link</a>"), "link");
    }

    #[test]
    fn extract_attr_works() {
        assert_eq!(
            extract_attr("<a href=\"https://example.com\">", "href"),
            Some("https://example.com".to_string())
        );
        assert_eq!(extract_attr("<b>no attr</b>", "href"), None);
    }

    #[test]
    fn extract_between_works() {
        assert_eq!(
            extract_between("<a>content</a>", ">", "</a>"),
            Some("content".to_string())
        );
        assert_eq!(
            extract_between("<b>bold</b>", ">", "</b>"),
            Some("bold".to_string())
        );
    }

    #[tokio::test]
    async fn rejects_empty_query() {
        let result = WebSearch.exec(json!({"query": ""})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_missing_query() {
        let result = WebSearch.exec(json!({})).await;
        assert!(result.is_err());
    }
}
