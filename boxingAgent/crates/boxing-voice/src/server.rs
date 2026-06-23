//! HTTP/SSE 服务端（axum）。
//!
//! 端点：
//! - GET  /health — 健康检查
//! - GET  /api/v1/voice-runtime/status — 完整状态
//! - GET  /api/v1/voice-runtime/events — SSE 事件流
//! - POST /api/v1/voice-runtime/audio/pcm — 提交 PCM 音频
//! - POST /api/v1/voice-runtime/simulate/wake — 模拟唤醒
//! - POST /api/v1/voice-runtime/simulate/transcript — 模拟转写
//! - POST /api/v1/voice-runtime/reset — 重置状态

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::trace::TraceLayer;

use crate::config::VoiceConfig;
use crate::runtime::{make_event, now_ms, VoiceEvent, VoicePhase, VoiceRuntime};

/// 应用共享状态。
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<VoiceConfig>,
    pub runtime: Arc<VoiceRuntime>,
    pub events: broadcast::Sender<VoiceEvent>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    healthy: bool,
    service: &'static str,
    version: &'static str,
    timestamp_ms: i64,
    state: &'static str,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    healthy: bool,
    service: &'static str,
    version: &'static str,
    timestamp_ms: i64,
    bind_addr: String,
    keyword: String,
    sample_rate: u32,
    engine: String,
    kws_model_dir: String,
    kws_model_exists: bool,
    asr_model_dir: Option<String>,
    asr_model_configured: bool,
    asr_language: String,
    ai_transcript_url: Option<String>,
    runtime: crate::runtime::RuntimeSnapshot,
}

#[derive(Debug, Deserialize)]
struct AudioPcmRequest {
    sample_rate: Option<u32>,
    pcm16_base64: String,
    #[serde(default)]
    final_chunk: bool,
}

#[derive(Debug, Serialize)]
struct AudioPcmResponse {
    status: &'static str,
    accepted: bool,
    state: &'static str,
    bytes_received: usize,
    sample_rate: u32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SimulateWakeRequest {
    keyword: Option<String>,
    confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct SimulateTranscriptRequest {
    text: String,
    confidence: Option<f32>,
    #[serde(default = "default_true")]
    final_result: bool,
}

fn default_true() -> bool {
    true
}

/// 构建 axum Router。
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/voice-runtime/health", get(health))
        .route("/api/v1/voice-runtime/status", get(status))
        .route("/api/v1/voice-runtime/events", get(events))
        .route("/api/v1/voice-runtime/audio/pcm", post(ingest_pcm))
        .route("/api/v1/voice-runtime/simulate/wake", post(simulate_wake))
        .route(
            "/api/v1/voice-runtime/simulate/transcript",
            post(simulate_transcript),
        )
        .route("/api/v1/voice-runtime/reset", post(reset))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 启动 HTTP 服务。
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let bind_addr = state.config.bind_addr;
    tracing::info!(
        %bind_addr,
        keyword = %state.config.keyword,
        sample_rate = state.config.sample_rate,
        engine = %state.config.engine,
        "voice runtime starting"
    );
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app(state)).await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let phase = state.runtime.current_phase().await;
    Json(HealthResponse {
        healthy: true,
        service: "boxing-voice",
        version: env!("CARGO_PKG_VERSION"),
        timestamp_ms: now_ms(),
        state: phase.as_str(),
    })
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let config = state.config.as_ref();
    Json(StatusResponse {
        healthy: true,
        service: "boxing-voice",
        version: env!("CARGO_PKG_VERSION"),
        timestamp_ms: now_ms(),
        bind_addr: config.bind_addr.to_string(),
        keyword: config.keyword.clone(),
        sample_rate: config.sample_rate,
        engine: config.engine.clone(),
        kws_model_dir: config.kws_model_dir.display().to_string(),
        kws_model_exists: config.kws_model_exists(),
        asr_model_dir: config
            .asr_model_dir
            .as_ref()
            .map(|p| p.display().to_string()),
        asr_model_configured: config.asr_model_exists(),
        asr_language: config.asr_language.clone(),
        ai_transcript_url: config.ai_transcript_url.clone(),
        runtime: state.runtime.snapshot().await,
    })
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|event| match event {
        Ok(event) => {
            let name = event.event_type.clone();
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Some(Ok(Event::default().event(name).data(payload)))
        }
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn ingest_pcm(
    State(state): State<AppState>,
    Json(request): Json<AudioPcmRequest>,
) -> Result<Json<AudioPcmResponse>, String> {
    let sample_rate = request.sample_rate.unwrap_or(state.config.sample_rate);
    if sample_rate != state.config.sample_rate {
        return Err(format!(
            "sample_rate must be {}, got {}",
            state.config.sample_rate, sample_rate
        ));
    }

    let pcm = general_purpose::STANDARD
        .decode(request.pcm16_base64.as_bytes())
        .map_err(|e| format!("invalid pcm16_base64: {e}"))?;

    state.runtime.inc_audio(1, pcm.len() as u64);

    let phase = state.runtime.current_phase().await;
    if matches!(phase, VoicePhase::Idle) {
        state.runtime.set_phase(VoicePhase::Listening).await;
    }

    // 唤醒词检测（需要 sherpa feature）
    #[cfg(feature = "sherpa")]
    if state.config.engine == "sherpa-onnx" {
        if let Ok(Some(detection)) = crate::kws::detect_keyword(&state.config, &pcm) {
            let session_id = state.runtime.start_session().await;
            state.runtime.inc_wake();
            state.runtime.set_phase(VoicePhase::WakeDetected).await;
            let mut event = make_event(
                &state.runtime,
                "wake_word",
                Some(session_id),
                VoicePhase::WakeDetected,
            );
            event.keyword = Some(detection.keyword);
            event.message = Some("wake word detected".into());
            publish(&state, event);
            state.runtime.set_phase(VoicePhase::Capturing).await;
        }
    }

    let capture_phase = state.runtime.current_phase().await;
    if matches!(
        capture_phase,
        VoicePhase::WakeDetected | VoicePhase::Capturing | VoicePhase::Transcribing
    ) {
        state.runtime.append_captured_pcm(&pcm).await;
    }

    if request.final_chunk {
        let captured = state.runtime.take_captured_pcm().await;
        let transcript: Option<String> =
            if state.config.engine == "sherpa-onnx" && !captured.is_empty() {
                #[cfg(feature = "sherpa")]
                {
                    crate::asr::recognize_speech(&state.config, &captured)
                        .ok()
                        .flatten()
                }
                #[cfg(not(feature = "sherpa"))]
                {
                    None
                }
            } else {
                None
            };

        if let Some(ref text) = transcript {
            if !text.trim().is_empty() {
                state.runtime.set_phase(VoicePhase::Completed).await;
                state.runtime.inc_transcript();
                let session_id = state.runtime.session_id().await;
                let mut event = make_event(
                    &state.runtime,
                    "transcript",
                    session_id.clone(),
                    VoicePhase::Completed,
                );
                event.transcript = Some(text.clone());
                event.final_result = true;
                publish(&state, event);
            }
        } else {
            let mut event = make_event(
                &state.runtime,
                "audio_final",
                state.runtime.session_id().await,
                state.runtime.current_phase().await,
            );
            event.message = Some("audio chunk stream reached final marker".into());
            publish(&state, event);
        }
    }

    Ok(Json(AudioPcmResponse {
        status: "success",
        accepted: true,
        state: state.runtime.current_phase().await.as_str(),
        bytes_received: pcm.len(),
        sample_rate,
        message: "PCM accepted".into(),
    }))
}

async fn simulate_wake(
    State(state): State<AppState>,
    Json(request): Json<SimulateWakeRequest>,
) -> Json<VoiceEvent> {
    let keyword = request
        .keyword
        .unwrap_or_else(|| state.config.keyword.clone());
    let confidence = request.confidence.unwrap_or(1.0);
    let session_id = state.runtime.start_session().await;
    state.runtime.inc_wake();
    state.runtime.set_phase(VoicePhase::WakeDetected).await;
    let mut event = make_event(
        &state.runtime,
        "wake_word",
        Some(session_id.clone()),
        VoicePhase::WakeDetected,
    );
    event.keyword = Some(keyword);
    event.confidence = Some(confidence);
    event.message = Some("wake word detected".into());
    publish(&state, event.clone());
    state.runtime.set_phase(VoicePhase::Capturing).await;
    Json(event)
}

async fn simulate_transcript(
    State(state): State<AppState>,
    Json(request): Json<SimulateTranscriptRequest>,
) -> Result<Json<VoiceEvent>, String> {
    let text = request.text.trim();
    if text.is_empty() {
        return Err("text is required".into());
    }
    let session_id = match state.runtime.session_id().await {
        Some(id) => id,
        None => state.runtime.start_session().await,
    };
    state.runtime.set_phase(VoicePhase::Transcribing).await;
    state.runtime.inc_transcript();
    let final_phase = if request.final_result {
        VoicePhase::Completed
    } else {
        VoicePhase::Transcribing
    };
    let mut event = make_event(&state.runtime, "transcript", Some(session_id), final_phase);
    event.transcript = Some(text.to_string());
    event.confidence = request.confidence;
    event.final_result = request.final_result;
    publish(&state, event.clone());
    state.runtime.set_phase(final_phase).await;
    Ok(Json(event))
}

async fn reset(State(state): State<AppState>) -> Json<VoiceEvent> {
    state.runtime.reset().await;
    let mut event = make_event(&state.runtime, "reset", None, VoicePhase::Idle);
    event.message = Some("voice runtime state reset".into());
    publish(&state, event.clone());
    Json(event)
}

fn publish(state: &AppState, event: VoiceEvent) {
    state.runtime.mark_event();
    let _ = state.events.send(event);
}
