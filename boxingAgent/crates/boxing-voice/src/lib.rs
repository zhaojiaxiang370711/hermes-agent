//! boxingAgent 语音运行时。
//!
//! 整合自 `/app/services/voice_runtime_rs/`，提供：
//! - 唤醒词检测（sherpa-onnx KeywordSpotter，默认 "小星"）
//! - 语音转文字 ASR（sherpa-onnx OfflineRecognizer + SenseVoice 模型）
//! - HTTP/SSE 服务端（axum，端口 50058）
//! - PCM 音频格式工具
//!
//! 模块化设计：
//! - `config` — 环境配置
//! - `runtime` — 语音状态机 + 事件
//! - `audio` — PCM 格式转换
//! - `asr` — sherpa-onnx ASR 后端（feature = "sherpa"）
//! - `kws` — sherpa-onnx 唤醒词后端（feature = "sherpa"）
//! - `server` — axum HTTP/SSE 服务端（feature = "server"）

pub mod audio;
pub mod config;
pub mod runtime;

#[cfg(feature = "sherpa")]
pub mod asr;

#[cfg(feature = "sherpa")]
pub mod kws;

#[cfg(feature = "server")]
pub mod server;

pub use config::VoiceConfig;
pub use runtime::{VoiceEvent, VoicePhase, VoiceRuntime};

/// 转写音频文件（16kHz mono PCM LE i16）为文本。
///
/// 需要 `sherpa` feature。如果 ASR 模型未配置，返回 Ok(None)。
#[cfg(feature = "sherpa")]
pub fn transcribe_pcm(config: &VoiceConfig, pcm_le_i16: &[u8]) -> anyhow::Result<Option<String>> {
    asr::recognize_speech(config, pcm_le_i16)
}

/// 检测唤醒词。
#[cfg(feature = "sherpa")]
pub fn detect_keyword(
    config: &VoiceConfig,
    pcm_le_i16: &[u8],
) -> anyhow::Result<Option<KeywordDetection>> {
    kws::detect_keyword(config, pcm_le_i16)
}

/// 唤醒词检测结果。
#[derive(Debug, Clone)]
pub struct KeywordDetection {
    pub keyword: String,
    pub confidence: Option<f32>,
}
