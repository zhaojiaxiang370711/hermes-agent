//! 火山（豆包）语音合成模型 2.0（doubao-seed-tts-2.0）provider — WebSocket 单向流式。
//!
//! 对等官方 demo：`X-Api-Key`（Ark API key）+ `X-Api-Resource-Id`(seed-tts-2.0) 鉴权，
//! `wss://.../api/v3/plan/tts/unidirectional/stream` 端点，发一帧 FullClientRequest，
//! 收帧拼音频直到 SessionFinished。二进制帧编解码见 proto.rs（与 demo protocols.py 同一套）。

use std::path::Path;

use boxing_config::env_value;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;

use super::proto::{marshal_request, unmarshal, Frame};
use super::VolcanoCfg;
use crate::ToolError;

/// 构造请求 JSON（对等官方 demo `body`：仅 `req_params`，无 user/additions）。
fn build_request(text: &str, voice: &str, cfg: &VolcanoCfg) -> Value {
    json!({
        "req_params": {
            "speaker": voice,
            "text": text,
            "audio_params": {
                "format": cfg.encoding,
                "sample_rate": cfg.sample_rate,
            }
        }
    })
}

/// 用火山 seed-tts-2.0 WS 单向流式合成 `text` 到 `out`。
pub async fn generate(
    text: &str,
    out: &Path,
    cfg: &VolcanoCfg,
    voice: &str,
    env_path: &Path,
) -> Result<(), ToolError> {
    let api_key = env_value(env_path, &cfg.api_key_env).ok_or_else(|| {
        ToolError::Other(format!(
            "火山 TTS 未配置 API key：在 {} 设置 {}",
            env_path.display(),
            cfg.api_key_env
        ))
    })?;
    let request = build_request(text, voice, cfg);
    let payload = serde_json::to_vec(&request)
        .map_err(|e| ToolError::Other(format!("序列化请求失败: {e}")))?;

    // 鉴权头：X-Api-Key（Ark key）+ X-Api-Resource-Id + 用量回传控制
    let mut req = cfg
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| ToolError::Other(format!("构造 WS 请求失败: {e}")))?;
    {
        let h = req.headers_mut();
        h.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(&api_key)
                .map_err(|_| ToolError::Other("非法 API key".into()))?,
        );
        h.insert(
            HeaderName::from_static("x-api-resource-id"),
            HeaderValue::from_str(&cfg.resource_id)
                .map_err(|_| ToolError::Other("非法 resource_id".into()))?,
        );
        h.insert(
            HeaderName::from_static("x-control-require-usage-tokens-return"),
            HeaderValue::from_static("*"),
        );
    }

    let (mut ws, _resp) = connect_async(req)
        .await
        .map_err(|e| ToolError::Other(format!("WS 连接失败: {e}")))?;

    ws.send(Message::Binary(marshal_request(&payload).into()))
        .await
        .map_err(|e| ToolError::Other(format!("WS 发送失败: {e}")))?;

    let mut audio: Vec<u8> = Vec::new();
    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| ToolError::Other(format!("WS 读取失败: {e}")))?;
        match msg {
            Message::Binary(bytes) => match unmarshal(&bytes)? {
                Frame::Audio(chunk) => audio.extend_from_slice(&chunk),
                Frame::SessionFinished => break,
                Frame::Error { code, payload } => {
                    return Err(ToolError::Other(format!(
                        "volcano error {code}: {}",
                        String::from_utf8_lossy(&payload)
                    )))
                }
                Frame::Other => {} // 忽略中间事件（如 session-started）
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    if audio.is_empty() {
        return Err(ToolError::Other("volcano 返回空音频".into()));
    }
    std::fs::write(out, &audio)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VolcanoCfg {
        VolcanoCfg {
            api_key_env: "VOLC_TTS_API_KEY".into(),
            resource_id: super::super::DEFAULT_VOLCANO_RESOURCE_ID.into(),
            encoding: "mp3".into(),
            sample_rate: 24000,
            endpoint: super::super::DEFAULT_VOLCANO_ENDPOINT.into(),
        }
    }

    #[test]
    fn request_shape_matches_seed_tts_demo() {
        let r = build_request("你好", "zh_female_t", &cfg());
        assert_eq!(r["req_params"]["speaker"], "zh_female_t");
        assert_eq!(r["req_params"]["text"], "你好");
        assert_eq!(r["req_params"]["audio_params"]["format"], "mp3");
        assert_eq!(r["req_params"]["audio_params"]["sample_rate"], 24000);
        // seed-tts-2.0 scheme：无 user / additions / enable_timestamp
        assert!(r.get("user").is_none());
        assert!(r["req_params"].get("additions").is_none());
        assert!(r["req_params"]["audio_params"]
            .get("enable_timestamp")
            .is_none());
    }

    /// Live smoke：真实火山 seed-tts-2.0 WS（需 ~/.hermes/.env 的 VOLC_TTS_API_KEY + config 的 tts.voice）。
    /// 设环境变量 VOLC_TTS_HOME 指向一个准备好 config.yaml/.env 的 hermes home，
    /// 然后 `cargo test live_volcano_tts_smoke -- --ignored --nocapture`。
    #[tokio::test]
    #[ignore = "live smoke: 需真实火山 Ark API key（VOLC_TTS_API_KEY）+ tts.voice"]
    async fn live_volcano_tts_smoke() {
        let home = std::path::PathBuf::from(std::env::var("VOLC_TTS_HOME").expect(
            "set VOLC_TTS_HOME to a hermes home with config.yaml + .env",
        ));
        let cfg = super::super::TtsConfig::load(&home).unwrap();
        assert!(matches!(cfg.provider, super::super::TtsProvider::Volcano));
        let voice = cfg.voice.clone().unwrap();
        let out = home.join("volcano-smoke.mp3");
        super::generate(
            "你好，这是一个火山语音合成的冒烟测试。",
            &out,
            &cfg.volcano,
            &voice,
            &home.join(".env"),
        )
        .await
        .expect("volcano generate failed");
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.len() > 1000, "audio too small: {} bytes", bytes.len());
    }
}
