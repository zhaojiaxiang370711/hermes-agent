//! sherpa-onnx ASR（语音转文字）后端。
//!
//! 使用 sherpa-onnx OfflineRecognizer + SenseVoice 模型，
//! 在 CPU 上进行本地推理，无需 API key。

use anyhow::{anyhow, Context, Result};

use crate::audio::pcm_i16_to_f32;
use crate::VoiceConfig;

/// 对 PCM 音频进行语音识别。
///
/// 需要 `QXZN_VOICE_RUNTIME_ASR_MODEL_DIR` 指向包含
/// `model.int8.onnx`（或 `model.onnx`）和 `tokens.txt` 的目录。
pub fn recognize_speech(config: &VoiceConfig, pcm_le_i16: &[u8]) -> Result<Option<String>> {
    let asr_dir = config
        .asr_model_dir
        .as_ref()
        .ok_or_else(|| anyhow!("QXZN_VOICE_RUNTIME_ASR_MODEL_DIR is not configured"))?;

    let model = ["model.int8.onnx", "model.onnx"]
        .iter()
        .map(|name| asr_dir.join(name))
        .find(|path| path.exists())
        .ok_or_else(|| anyhow!("ASR model not found in: {}", asr_dir.display()))?;

    let tokens = asr_dir.join("tokens.txt");
    if !tokens.exists() {
        anyhow::bail!("tokens.txt not found in: {}", asr_dir.display());
    }

    let mut recognizer_config = sherpa_onnx::OfflineRecognizerConfig::default();
    recognizer_config.feat_config.sample_rate = config.sample_rate as i32;
    recognizer_config.model_config.sense_voice = sherpa_onnx::OfflineSenseVoiceModelConfig {
        model: Some(model.display().to_string()),
        language: Some(config.asr_language.clone()),
        use_itn: true,
    };
    recognizer_config.model_config.tokens = Some(tokens.display().to_string());
    recognizer_config.model_config.provider = Some("cpu".to_string());
    recognizer_config.model_config.num_threads = 2;

    let recognizer = sherpa_onnx::OfflineRecognizer::create(&recognizer_config)
        .ok_or_else(|| anyhow!("failed to create sherpa-onnx OfflineRecognizer"))?;
    let stream = recognizer.create_stream();
    let samples = pcm_i16_to_f32(pcm_le_i16).context("convert PCM to f32")?;
    stream.accept_waveform(config.sample_rate as i32, &samples);
    recognizer.decode(&stream);
    Ok(stream.get_result().map(|result| result.text))
}
