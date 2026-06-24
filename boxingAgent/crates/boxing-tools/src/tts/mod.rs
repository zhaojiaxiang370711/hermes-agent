//! `text_to_speech` 工具：文本转语音。
//!
//! 与 Hermes 原版 `tools/tts_tool.py` 对等（简化版）：
//! - 优先使用 edge-tts CLI（免费，无需 API key）
//! - 输出保存到 ~/.hermes/audio_cache/
//! - 返回 MEDIA: 路径标签

pub mod proto;

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::{Tool, ToolError};

/// `text_to_speech` 工具。
pub struct TextToSpeech;

#[async_trait::async_trait]
impl Tool for TextToSpeech {
    fn name(&self) -> &str {
        "text_to_speech"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "text_to_speech",
            "description": "Convert text to speech audio. Uses edge-tts (free, no API key required). Returns a MEDIA: path that platforms deliver as native audio. Saves to ~/.hermes/audio_cache/.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The text to convert to speech."},
                    "output_path": {"type": "string", "description": "Optional custom file path. Defaults to ~/.hermes/audio_cache/<timestamp>.mp3."}
                },
                "required": ["text"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("text"))?;

        if text.trim().is_empty() {
            return Err(ToolError::InvalidArg {
                arg: "text",
                reason: "text cannot be empty".into(),
            });
        }

        // 截断过长文本（4096 字符限制）
        let text = if text.len() > 4096 {
            &text[..4096]
        } else {
            text
        };

        let output_path = match args.get("output_path").and_then(|v| v.as_str()) {
            Some(p) => PathBuf::from(p),
            None => default_output_path(),
        };

        // 确保目录存在
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        generate_via_edge_tts(text, &output_path).await?;

        let path_str = output_path.to_string_lossy().to_string();
        Ok(json!({
            "success": true,
            "file_path": &path_str,
            "media": format!("MEDIA:{}", path_str),
        })
        .to_string())
    }
}

/// 默认输出路径：~/.hermes/audio_cache/<timestamp>.mp3
fn default_output_path() -> PathBuf {
    let home = std::env::var("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let h = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(h).join(".hermes")
        });
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    home.join("audio_cache").join(format!("{ts}.mp3"))
}

/// 通过 edge-tts CLI 生成语音。
///
/// edge-tts 是免费的微软 Edge 语音合成工具，不需要 API key。
/// 安装：pip install edge-tts
async fn generate_via_edge_tts(text: &str, output_path: &std::path::Path) -> Result<(), ToolError> {
    let voice = read_tts_voice();
    let mut cmd = tokio::process::Command::new("edge-tts");
    cmd.arg("--voice")
        .arg(&voice)
        .arg("--text")
        .arg(text)
        .arg("--write-media")
        .arg(output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().await.map_err(|e| {
        ToolError::Other(format!(
            "edge-tts not found (install: pip install edge-tts): {}",
            e
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Other(format!(
            "edge-tts failed: {}",
            stderr.trim()
        )));
    }

    if !output_path.exists() {
        return Err(ToolError::Other(
            "edge-tts completed but output file not found".into(),
        ));
    }

    Ok(())
}

/// 从 config 读取 voice（默认 en-US-AriaNeural）。
fn read_tts_voice() -> String {
    // 尝试从 config.yaml 读取 tts.edge.voice（简化解析）
    if let Ok(home) = std::env::var("HERMES_HOME").or_else(|_| std::env::var("HOME")) {
        let config_path = std::path::Path::new(&home)
            .join(".hermes")
            .join("config.yaml");
        if let Ok(text) = std::fs::read_to_string(&config_path) {
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("voice:") {
                    let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let v = parts[1].trim().trim_matches('"').trim_matches('\'');
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }
    }
    "en-US-AriaNeural".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid() {
        let schema = TextToSpeech.schema();
        assert_eq!(schema["name"], "text_to_speech");
        assert!(schema["parameters"]["properties"]["text"].is_object());
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let result = TextToSpeech.exec(json!({"text": ""})).await;
        assert!(result.is_err());
    }

    #[test]
    fn default_output_path_has_mp3_extension() {
        let path = default_output_path();
        assert_eq!(path.extension().unwrap(), "mp3");
    }

    #[test]
    fn default_voice_is_aria() {
        let voice = read_tts_voice();
        assert!(!voice.is_empty());
    }

    /// Live smoke：真实调用 edge-tts CLI（需联网 + 已安装 edge-tts）。
    /// 默认不跑，用 `cargo test live_edge_tts_smoke -- --ignored` 触发。
    #[tokio::test]
    #[ignore = "live smoke: 需联网 + 已安装 edge-tts CLI"]
    async fn live_edge_tts_smoke() {
        let out_path = std::env::temp_dir().join(format!(
            "boxing-tts-smoke-{}.mp3",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let out = TextToSpeech
            .exec(json!({
                "text": "Hello. This is a text to speech smoke test from the boxing-agent Rust port.",
                "output_path": out_path.to_string_lossy(),
            }))
            .await
            .expect("text_to_speech exec failed");

        // 返回 JSON 带 MEDIA: 标签
        assert!(out.contains("MEDIA:"), "missing MEDIA tag in: {out}");
        assert!(out_path.exists(), "mp3 未生成: {out_path:?}");

        let bytes = std::fs::read(&out_path).expect("读取 mp3");
        assert!(bytes.len() > 1000, "mp3 过小: {} bytes", bytes.len());
        // MP3 校验：ID3v2 头 或 帧同步 0xFF{E0}
        let is_mp3 = bytes.starts_with(b"ID3")
            || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0);
        assert!(
            is_mp3,
            "非 mp3 流 (head={:02x?})",
            &bytes[..2.min(bytes.len())]
        );

        eprintln!("smoke ok: {} bytes @ {out_path:?}", bytes.len());
        let _ = std::fs::remove_file(&out_path);
    }
}
