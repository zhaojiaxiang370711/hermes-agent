//! 火山（豆包）TTS provider — WebSocket 单向流式（对等 demo）。
//!
//! 流程对等 demo `examples/volcengine/unidirectional_stream.py`：解析凭证 ->
//! 推导 resource_id -> 连 WS（X-Api-* 头）-> 发一帧 FullClientRequest ->
//! 收帧累积音频直到 SessionFinished -> 写文件。二进制帧编解码见 proto.rs。

use std::path::Path;

use boxing_config::env_value;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::proto::{marshal_request, unmarshal, Frame};
use super::VolcanoCfg;
use crate::ToolError;

/// 按 voice 前缀推导 resource_id（对等 demo `get_resource_id`）。
fn derive_resource_id(voice: &str, override_: &str) -> String {
    if !override_.is_empty() {
        return override_.into();
    }
    if voice.starts_with("S_") {
        "volc.megatts.default".into()
    } else {
        "volc.service_type.10029".into()
    }
}

/// 构造请求 JSON（对等 demo `request`）。
fn build_request(text: &str, voice: &str, cfg: &VolcanoCfg) -> Value {
    json!({
        "user": { "uid": Uuid::new_v4().to_string() },
        "req_params": {
            "speaker": voice,
            "audio_params": {
                "format": cfg.encoding,
                "sample_rate": cfg.sample_rate,
                "enable_timestamp": true,
            },
            "text": text,
            "additions": "{\"disable_markdown_filter\":false}",
        }
    })
}

/// 用火山 WS 单向流式合成 `text` 到 `out`。
pub async fn generate(
    text: &str,
    out: &Path,
    cfg: &VolcanoCfg,
    voice: &str,
    env_path: &Path,
) -> Result<(), ToolError> {
    let appid = env_value(env_path, &cfg.appid_env).ok_or_else(|| {
        ToolError::Other(format!(
            "火山 TTS 未配置 appid：在 {} 设置 {}",
            env_path.display(),
            cfg.appid_env
        ))
    })?;
    let token = env_value(env_path, &cfg.token_env).ok_or_else(|| {
        ToolError::Other(format!(
            "火山 TTS 未配置 token：在 {} 设置 {}",
            env_path.display(),
            cfg.token_env
        ))
    })?;
    let resource_id = derive_resource_id(voice, &cfg.resource_id);
    let connect_id = Uuid::new_v4().to_string();
    let request = build_request(text, voice, cfg);
    let payload = serde_json::to_vec(&request)
        .map_err(|e| ToolError::Other(format!("序列化请求失败: {e}")))?;

    // 带 X-Api-* 握手头
    let mut req = cfg
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| ToolError::Other(format!("构造 WS 请求失败: {e}")))?;
    {
        let h = req.headers_mut();
        for (k, v) in [
            ("X-Api-App-Key", appid.as_str()),
            ("X-Api-Access-Key", token.as_str()),
            ("X-Api-Resource-Id", resource_id.as_str()),
            ("X-Api-Connect-Id", connect_id.as_str()),
        ] {
            h.insert(
                k.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(v)
                    .map_err(|_| ToolError::Other(format!("非法 header 值: {k}")))?,
            );
        }
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

    fn cfg(resource_id: &str) -> VolcanoCfg {
        VolcanoCfg {
            appid_env: "VOLC_TTS_APPID".into(),
            token_env: "VOLC_TTS_TOKEN".into(),
            resource_id: resource_id.into(),
            encoding: "mp3".into(),
            sample_rate: 24000,
            endpoint: super::super::DEFAULT_VOLCANO_ENDPOINT.into(),
        }
    }

    #[test]
    fn resource_id_default_for_doubao_voice() {
        assert_eq!(
            derive_resource_id("zh_female_xxx", ""),
            "volc.service_type.10029"
        );
    }

    #[test]
    fn resource_id_mega_for_s_prefix() {
        assert_eq!(derive_resource_id("S_abc", ""), "volc.megatts.default");
    }

    #[test]
    fn resource_id_override_wins() {
        assert_eq!(
            derive_resource_id("zh_female_xxx", "volc.custom"),
            "volc.custom"
        );
        assert_eq!(derive_resource_id("S_abc", "volc.custom"), "volc.custom");
    }

    #[test]
    fn request_shape_matches_demo() {
        let c = cfg("");
        let r = build_request("你好", "zh_female_t", &c);
        assert_eq!(r["req_params"]["speaker"], "zh_female_t");
        assert_eq!(r["req_params"]["text"], "你好");
        assert_eq!(r["req_params"]["audio_params"]["format"], "mp3");
        assert_eq!(r["req_params"]["audio_params"]["sample_rate"], 24000);
        assert_eq!(r["req_params"]["audio_params"]["enable_timestamp"], true);
        assert_eq!(
            r["req_params"]["additions"],
            "{\"disable_markdown_filter\":false}"
        );
        assert!(r["user"]["uid"].is_string());
    }

    /// Live smoke：真实火山 WS（需 ~/.hermes/.env 的 VOLC_TTS_APPID/TOKEN + config 的 tts.voice）。
    /// 设环境变量 VOLC_TTS_HOME 指向一个准备好 config.yaml/.env 的 hermes home，
    /// 然后 `cargo test live_volcano_tts_smoke -- --ignored --nocapture`。
    #[tokio::test]
    #[ignore = "live smoke: 需真实火山凭证（VOLC_TTS_APPID/TOKEN）+ tts.voice"]
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
