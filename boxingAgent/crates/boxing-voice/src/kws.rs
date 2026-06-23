//! sherpa-onnx 唤醒词检测（KWS）后端。
//!
//! 使用 sherpa-onnx KeywordSpotter 检测唤醒词（默认 "小星"）。

use anyhow::{anyhow, Context, Result};

use crate::audio::pcm_i16_to_f32;
use crate::{KeywordDetection, VoiceConfig};

/// 从 PCM 音频中检测唤醒词。
pub fn detect_keyword(config: &VoiceConfig, pcm_le_i16: &[u8]) -> Result<Option<KeywordDetection>> {
    let mut kws_config = sherpa_onnx::KeywordSpotterConfig::default();
    kws_config.model_config.transducer.encoder = Some(
        config
            .kws_model_dir
            .join("encoder-epoch-99-avg-1-chunk-16-left-64.int8.onnx")
            .display()
            .to_string(),
    );
    kws_config.model_config.transducer.decoder = Some(
        config
            .kws_model_dir
            .join("decoder-epoch-99-avg-1-chunk-16-left-64.int8.onnx")
            .display()
            .to_string(),
    );
    kws_config.model_config.transducer.joiner = Some(
        config
            .kws_model_dir
            .join("joiner-epoch-99-avg-1-chunk-16-left-64.int8.onnx")
            .display()
            .to_string(),
    );
    kws_config.model_config.tokens = Some(
        config
            .kws_model_dir
            .join("tokens.txt")
            .display()
            .to_string(),
    );
    kws_config.model_config.provider = Some("cpu".to_string());
    kws_config.model_config.num_threads = 2;
    kws_config.keywords_file = Some(config.kws_keywords_file.display().to_string());

    let kws = sherpa_onnx::KeywordSpotter::create(&kws_config)
        .ok_or_else(|| anyhow!("failed to create sherpa-onnx KeywordSpotter"))?;
    let stream = kws.create_stream();
    let samples = pcm_i16_to_f32(pcm_le_i16).context("convert PCM to f32")?;
    let sample_rate = config.sample_rate as i32;
    stream.accept_waveform(sample_rate, &samples);
    stream.accept_waveform(
        sample_rate,
        &vec![0.0f32; (config.sample_rate / 2) as usize],
    );
    stream.input_finished();

    while kws.is_ready(&stream) {
        kws.decode(&stream);
        if let Some(result) = kws.get_result(&stream) {
            if !result.keyword.is_empty() {
                return Ok(Some(KeywordDetection {
                    keyword: result.keyword,
                    confidence: None,
                }));
            }
        }
    }

    Ok(None)
}
