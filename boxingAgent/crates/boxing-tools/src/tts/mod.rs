//! `text_to_speech` 工具：文本转语音。
//!
//! 对等 Hermes `tools/tts_tool.py` 的 strategy-by-name 分发（源码零 class）：
//! `tts.provider` 选 edge 或 volcano，`match` 分发到对应模块。
//! - edge：edge-tts CLI（免费）
//! - volcano：豆包 WebSocket 单向流式（见 volcano.rs / proto.rs）
//!
//! 工具对外参数不变：`text` + 可选 `output_path`，返回 `MEDIA:` 路径。

pub mod edge;
pub mod proto;
pub mod volcano;

use std::path::{Path, PathBuf};

use boxing_config::{load_or_default, ConfigDoc};
use serde_json::{json, Value};

use crate::{Tool, ToolError};

/// 默认火山 WS 端点（单向流式）。
pub const DEFAULT_VOLCANO_ENDPOINT: &str =
    "wss://openspeech.bytedance.com/api/v3/tts/unidirectional/stream";

/// `text_to_speech` 工具。持有 hermes home 以读取 config.yaml / .env。
pub struct TextToSpeech {
    home: PathBuf,
}

impl TextToSpeech {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }
}

/// 可选 provider。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsProvider {
    Edge,
    Volcano,
}

/// 火山 provider 配置（对等 config 的 `tts.volcano.*`）。
#[derive(Debug, Clone)]
pub struct VolcanoCfg {
    pub appid_env: String,
    pub token_env: String,
    pub resource_id: String, // "" -> 按 voice 前缀推导
    pub encoding: String,
    pub sample_rate: u32,
    pub endpoint: String,
}

/// 解析后的 TTS 配置。
#[derive(Debug, Clone)]
pub struct TtsConfig {
    pub provider: TtsProvider,
    pub voice: Option<String>,
    pub volcano: VolcanoCfg,
}

impl TtsConfig {
    /// 从 `home/config.yaml` 加载（缺失视作默认 edge）。
    pub fn load(home: &Path) -> Result<Self, ToolError> {
        let doc = load_or_default(&home.join("config.yaml"))
            .map_err(|e| ToolError::Other(format!("读取 config.yaml 失败: {e}")))?;

        let provider = match get_opt(&doc, "tts.provider").as_deref() {
            Some("volcano") => TtsProvider::Volcano,
            _ => TtsProvider::Edge, // 默认 + 未知名都走 edge
        };
        let voice = get_opt(&doc, "tts.voice");
        let volcano = VolcanoCfg {
            appid_env: get_opt(&doc, "tts.volcano.appid_env")
                .unwrap_or_else(|| "VOLC_TTS_APPID".into()),
            token_env: get_opt(&doc, "tts.volcano.token_env")
                .unwrap_or_else(|| "VOLC_TTS_TOKEN".into()),
            resource_id: get_opt(&doc, "tts.volcano.resource_id").unwrap_or_default(),
            encoding: get_opt(&doc, "tts.volcano.encoding").unwrap_or_else(|| "mp3".into()),
            sample_rate: get_opt(&doc, "tts.volcano.sample_rate")
                .and_then(|s| s.parse().ok())
                .unwrap_or(24000),
            endpoint: get_opt(&doc, "tts.volcano.endpoint")
                .unwrap_or_else(|| DEFAULT_VOLCANO_ENDPOINT.into()),
        };
        Ok(Self {
            provider,
            voice,
            volcano,
        })
    }

    /// edge 用：voice 缺省 en-US-AriaNeural。
    pub fn voice_or_edge_default(&self) -> String {
        self.voice
            .clone()
            .unwrap_or_else(|| "en-US-AriaNeural".into())
    }
}

/// ConfigDoc::get 的 Option 包装（NotFound -> None）。
fn get_opt(doc: &ConfigDoc, dotted: &str) -> Option<String> {
    doc.get(dotted).ok().filter(|s| !s.is_empty())
}

#[async_trait::async_trait]
impl Tool for TextToSpeech {
    fn name(&self) -> &str {
        "text_to_speech"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "text_to_speech",
            "description": "Convert text to speech audio. Provider (edge or volcano) and voice are set in ~/.hermes/config.yaml under tts.provider / tts.voice. Returns a MEDIA: path that platforms deliver as native audio.",
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
        // 截断过长文本（~4096 字节，按 UTF-8 字符边界对齐，避免 panic）
        let text = if text.len() > 4096 {
            let mut end = 4096;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };

        let cfg = TtsConfig::load(&self.home)?;
        let out = match args.get("output_path").and_then(|v| v.as_str()) {
            Some(p) => PathBuf::from(p),
            None => default_output_path(&self.home),
        };
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let env_path = self.home.join(".env");
        match cfg.provider {
            TtsProvider::Edge => {
                edge::generate(text, &out, &cfg.voice_or_edge_default()).await?;
            }
            TtsProvider::Volcano => {
                let voice = cfg.voice.clone().ok_or_else(|| {
                    ToolError::Other("火山 TTS 需在 config 设置 tts.voice (voice_type)".into())
                })?;
                volcano::generate(text, &out, &cfg.volcano, &voice, &env_path).await?;
            }
        }

        let p = out.to_string_lossy().to_string();
        Ok(
            json!({"success": true, "file_path": &p, "media": format!("MEDIA:{p}")})
                .to_string(),
        )
    }
}

/// 默认输出路径：`<home>/audio_cache/<timestamp>.mp3`。
fn default_output_path(home: &Path) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    home.join("audio_cache").join(format!("{ts}.mp3"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boxing-tts-cfg-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(home: &Path, yaml: &str) {
        let mut f = std::fs::File::create(home.join("config.yaml")).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
    }

    #[test]
    fn defaults_to_edge_when_unconfigured() {
        let home = tmp_home("default");
        let cfg = TtsConfig::load(&home).unwrap();
        assert!(matches!(cfg.provider, TtsProvider::Edge));
        assert_eq!(cfg.voice_or_edge_default(), "en-US-AriaNeural");
    }

    #[test]
    fn parses_volcano_provider() {
        let home = tmp_home("volcano");
        write_config(
            &home,
            "tts:\n  provider: volcano\n  voice: zh_female_test\n  volcano:\n    encoding: wav\n    sample_rate: 16000\n",
        );
        let cfg = TtsConfig::load(&home).unwrap();
        assert!(matches!(cfg.provider, TtsProvider::Volcano));
        assert_eq!(cfg.voice.as_deref(), Some("zh_female_test"));
        assert_eq!(cfg.volcano.encoding, "wav");
        assert_eq!(cfg.volcano.sample_rate, 16000);
        assert_eq!(cfg.volcano.appid_env, "VOLC_TTS_APPID"); // default
        assert_eq!(cfg.volcano.endpoint, DEFAULT_VOLCANO_ENDPOINT);
    }

    #[tokio::test]
    async fn dispatch_volcano_arm_reports_missing_creds() {
        // provider=volcano 但无 .env 凭证：dispatch 应到达真实 volcano 客户端并报缺凭证
        let home = tmp_home("dispatch");
        write_config(&home, "tts:\n  provider: volcano\n  voice: v1\n");
        let tool = TextToSpeech::new(home);
        let out = tool
            .exec(json!({"text": "hi", "output_path": tmp_home("dispatchout").join("o.mp3").to_string_lossy()}))
            .await;
        assert!(out.is_err());
        let msg = out.unwrap_err().to_string();
        assert!(msg.contains("未配置"), "expected missing-creds error, got: {msg}");
    }

    #[test]
    fn schema_name_and_text_param() {
        let home = tmp_home("schema");
        let tool = TextToSpeech::new(home);
        let s = tool.schema();
        assert_eq!(s["name"], "text_to_speech");
        assert!(s["parameters"]["properties"]["text"].is_object());
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let home = tmp_home("empty");
        let tool = TextToSpeech::new(home);
        assert!(tool.exec(json!({"text": ""})).await.is_err());
    }

    #[test]
    fn default_output_path_is_mp3_under_home() {
        let home = tmp_home("outpath");
        let p = default_output_path(&home);
        assert!(p.starts_with(&home));
        assert_eq!(p.extension().unwrap(), "mp3");
    }

    /// Live smoke：真实 edge-tts（需联网 + 已安装 edge-tts）。
    /// `cargo test live_edge_tts_smoke -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live smoke: 需联网 + 已安装 edge-tts CLI"]
    async fn live_edge_tts_smoke() {
        let home = tmp_home("live");
        write_config(&home, "tts:\n  provider: edge\n");
        let out = home.join("smoke.mp3");
        let res = TextToSpeech::new(home)
            .exec(json!({
                "text": "Hello. Edge smoke test through the dispatch path.",
                "output_path": out.to_string_lossy(),
            }))
            .await
            .expect("edge exec failed");
        assert!(res.contains("MEDIA:"), "missing MEDIA tag: {res}");
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.len() > 1000, "mp3 too small: {} bytes", bytes.len());
        let is_mp3 = bytes.starts_with(b"ID3")
            || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0);
        assert!(is_mp3, "not an mp3 stream");
    }
}
