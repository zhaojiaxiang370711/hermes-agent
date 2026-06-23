//! `image_generate` 工具：通过 FAL.ai 生成图像。
//!
//! 与 Hermes 原版 `tools/image_generation_tool.py` 对等（简化版）。

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Tool, ToolError};

const VALID_ASPECT_RATIOS: &[&str] = &["landscape", "square", "portrait"];

/// `image_generate` 工具：文本生成图像。
pub struct ImageGenerate {
    api_key: String,
    model: String,
}

impl ImageGenerate {
    pub fn new() -> Self {
        let key = read_env_value("FAL_KEY")
            .or_else(|| read_env_value("FAL_API_KEY"))
            .unwrap_or_default();
        Self {
            api_key: key,
            model: "fal-ai/flux/schnell".to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

impl Default for ImageGenerate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for ImageGenerate {
    fn name(&self) -> &str {
        "image_generate"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "image_generate",
            "description": "Generate images from text prompts using FAL.ai. Returns JSON with image URL.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Text prompt describing the desired image."},
                    "aspect_ratio": {"type": "string", "enum": VALID_ASPECT_RATIOS, "default": "landscape"}
                },
                "required": ["prompt"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("prompt"))?;

        if prompt.trim().is_empty() {
            return Err(ToolError::InvalidArg {
                arg: "prompt",
                reason: "prompt cannot be empty".into(),
            });
        }

        if self.api_key.is_empty() {
            return Err(ToolError::Other(
                "FAL API key not configured (set FAL_KEY)".into(),
            ));
        }

        let aspect = args
            .get("aspect_ratio")
            .and_then(|v| v.as_str())
            .unwrap_or("landscape");
        let image_url = generate_via_fal(&self.api_key, &self.model, prompt, aspect).await?;
        Ok(json!({"success": true, "image": image_url, "model": &self.model}).to_string())
    }
}

async fn generate_via_fal(
    api_key: &str,
    model: &str,
    prompt: &str,
    aspect_ratio: &str,
) -> Result<String, ToolError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| ToolError::Other(format!("HTTP client: {}", e)))?;

    let submit_url = format!("https://queue.fal.run/{}", model);
    let payload = build_fal_payload(prompt, aspect_ratio);

    let resp = client
        .post(&submit_url)
        .header("Authorization", format!("Key {}", api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| ToolError::Other(format!("FAL submit: {}", e)))?;

    #[derive(Deserialize)]
    struct SubmitResp {
        request_id: Option<String>,
    }
    let body: SubmitResp = resp
        .json()
        .await
        .map_err(|e| ToolError::Other(format!("FAL parse: {}", e)))?;

    let request_id = body
        .request_id
        .ok_or_else(|| ToolError::Other("FAL: no request_id".into()))?;

    let status_url = format!(
        "https://queue.fal.run/{}/requests/{}/status",
        model, request_id
    );
    let result_url = format!("https://queue.fal.run/{}/requests/{}", model, request_id);

    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let status_resp = client
            .get(&status_url)
            .header("Authorization", format!("Key {}", api_key))
            .send()
            .await
            .map_err(|e| ToolError::Other(format!("FAL status: {}", e)))?;

        #[derive(Deserialize)]
        struct StatusResp {
            status: String,
            #[serde(default)]
            error: Option<String>,
        }
        let st: StatusResp = match status_resp.json().await {
            Ok(s) => s,
            Err(_) => continue,
        };

        match st.status.as_str() {
            "COMPLETED" => break,
            "FAILED" => {
                return Err(ToolError::Other(format!(
                    "FAL failed: {}",
                    st.error.unwrap_or_default()
                )))
            }
            _ => continue,
        }
    }

    let result_resp = client
        .get(&result_url)
        .header("Authorization", format!("Key {}", api_key))
        .send()
        .await
        .map_err(|e| ToolError::Other(format!("FAL result: {}", e)))?;

    let result: Value = result_resp
        .json()
        .await
        .map_err(|e| ToolError::Other(format!("FAL parse result: {}", e)))?;

    let url = result
        .get("images")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            result
                .get("image")
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| result.get("image").and_then(|v| v.as_str()))
        .ok_or_else(|| ToolError::Other("FAL: no image URL in result".into()))?;

    Ok(url.to_string())
}

fn build_fal_payload(prompt: &str, aspect_ratio: &str) -> Value {
    let size = match aspect_ratio {
        "portrait" => "864x1536",
        "square" => "1024x1024",
        _ => "1536x864",
    };
    json!({"prompt": prompt, "image_size": size, "num_images": 1})
}

fn read_env_value(key: &str) -> Option<String> {
    if let Ok(val) = std::env::var(key) {
        if !val.is_empty() {
            return Some(val);
        }
    }
    if let Ok(home) = std::env::var("HERMES_HOME").or_else(|_| std::env::var("HOME")) {
        let env_path = std::path::Path::new(&home).join(".hermes").join(".env");
        if let Ok(text) = std::fs::read_to_string(&env_path) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == key {
                        let val = v.trim().trim_matches('"').trim_matches('\'');
                        if !val.is_empty() {
                            return Some(val.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid() {
        let tool = ImageGenerate::new();
        let schema = tool.schema();
        assert_eq!(schema["name"], "image_generate");
    }

    #[test]
    fn build_payload_landscape() {
        let payload = build_fal_payload("a cat", "landscape");
        assert_eq!(payload["image_size"], "1536x864");
    }

    #[tokio::test]
    async fn rejects_empty_prompt() {
        let tool = ImageGenerate::new();
        let result = tool.exec(json!({"prompt": ""})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_missing_api_key() {
        let tool = ImageGenerate {
            api_key: String::new(),
            model: "test".into(),
        };
        let result = tool.exec(json!({"prompt": "a cat"})).await;
        assert!(result.is_err());
    }

    #[test]
    fn read_env_finds_key() {
        std::env::set_var("BOXING_TEST_FAL", "test-key-123");
        assert_eq!(
            read_env_value("BOXING_TEST_FAL"),
            Some("test-key-123".into())
        );
        std::env::remove_var("BOXING_TEST_FAL");
    }
}
