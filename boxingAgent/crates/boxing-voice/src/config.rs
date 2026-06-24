//! 语音运行时配置。

use std::net::SocketAddr;
use std::path::PathBuf;

const DEFAULT_PORT: u16 = 50058;
const DEFAULT_SAMPLE_RATE: u32 = 16_000;
const DEFAULT_KEYWORD: &str = "小星";

/// 语音运行时配置（从环境变量读取）。
#[derive(Debug, Clone)]
pub struct VoiceConfig {
    pub bind_addr: SocketAddr,
    pub keyword: String,
    pub sample_rate: u32,
    pub kws_model_dir: PathBuf,
    pub kws_keywords_file: PathBuf,
    pub asr_model_dir: Option<PathBuf>,
    pub asr_language: String,
    pub ai_transcript_url: Option<String>,
    pub engine: String,
}

impl VoiceConfig {
    /// 从环境变量构建配置。
    pub fn from_env() -> anyhow::Result<Self> {
        let port = env_u16("QXZN_VOICE_RUNTIME_PORT", DEFAULT_PORT);
        let host = std::env::var("QXZN_VOICE_RUNTIME_HOST")
            .or_else(|_| std::env::var("QXZN_VOICE_RUNTIME_BIND_HOST"))
            .unwrap_or_else(|_| "0.0.0.0".to_string());
        let bind_addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| anyhow::anyhow!("parse bind address {host}:{port}: {e}"))?;

        let app_dir = resolve_app_dir();
        let kws_model_dir = env_path(
            "QXZN_VOICE_RUNTIME_KWS_MODEL_DIR",
            app_dir.join("services/ai_agent/models/kws/sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01"),
        );
        let kws_keywords_file = env_path(
            "QXZN_VOICE_RUNTIME_KWS_KEYWORDS",
            app_dir.join("services/ai_agent/models/kws/keywords_custom.txt"),
        );
        let asr_model_dir = std::env::var("QXZN_VOICE_RUNTIME_ASR_MODEL_DIR")
            .ok()
            .map(PathBuf::from);

        Ok(Self {
            bind_addr,
            keyword: std::env::var("QXZN_VOICE_RUNTIME_KEYWORD")
                .unwrap_or_else(|_| DEFAULT_KEYWORD.to_string()),
            sample_rate: env_u32("QXZN_VOICE_RUNTIME_SAMPLE_RATE", DEFAULT_SAMPLE_RATE),
            kws_model_dir,
            kws_keywords_file,
            asr_model_dir,
            asr_language: std::env::var("QXZN_VOICE_RUNTIME_ASR_LANGUAGE")
                .unwrap_or_else(|_| "zh".to_string()),
            ai_transcript_url: std::env::var("QXZN_VOICE_RUNTIME_AI_TRANSCRIPT_URL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            engine: std::env::var("QXZN_VOICE_RUNTIME_ENGINE")
                .unwrap_or_else(|_| "stub".to_string()),
        })
    }

    /// KWS 模型文件是否齐全。
    pub fn kws_model_exists(&self) -> bool {
        [
            "tokens.txt",
            "encoder-epoch-99-avg-1-chunk-16-left-64.int8.onnx",
        ]
        .iter()
        .all(|name| self.kws_model_dir.join(name).exists())
    }

    /// ASR 模型是否已配置且存在。
    pub fn asr_model_exists(&self) -> bool {
        self.asr_model_dir.as_ref().is_some_and(|p| p.exists())
    }
}

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_path(name: &str, default: PathBuf) -> PathBuf {
    std::env::var(name).map(PathBuf::from).unwrap_or(default)
}

fn resolve_app_dir() -> PathBuf {
    if let Ok(path) = std::env::var("APP_DIR") {
        return PathBuf::from(path);
    }
    for path in [
        "/home/x/code/qxzn02/app",
        "/home/xwsl/code/qxzn02/app",
        "/home/radxa/qxzn02/app",
    ] {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_is_correct() {
        assert_eq!(DEFAULT_PORT, 50058);
    }

    #[test]
    fn default_keyword_is_chinese() {
        assert_eq!(DEFAULT_KEYWORD, "小星");
    }
}
