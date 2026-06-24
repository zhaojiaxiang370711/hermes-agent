//! 语音运行时状态机 + 事件系统。

use serde::Serialize;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// 语音运行阶段。
#[derive(Debug, Clone, Copy, Default)]
pub enum VoicePhase {
    #[default]
    Idle,
    Listening,
    WakeDetected,
    Capturing,
    Transcribing,
    Responding,
    Speaking,
    Completed,
}

impl VoicePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::WakeDetected => "wake_detected",
            Self::Capturing => "capturing",
            Self::Transcribing => "transcribing",
            Self::Responding => "responding",
            Self::Speaking => "speaking",
            Self::Completed => "completed",
        }
    }
}

/// 语音事件（SSE 推送）。
#[derive(Debug, Serialize, Clone)]
pub struct VoiceEvent {
    pub id: u64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub timestamp_ms: i64,
    pub session_id: Option<String>,
    pub state: String,
    pub keyword: Option<String>,
    pub transcript: Option<String>,
    pub assistant_text: Option<String>,
    pub media: Option<String>,
    pub file_path: Option<String>,
    pub confidence: Option<f32>,
    pub final_result: bool,
    pub message: Option<String>,
}

/// 运行时状态。
pub struct VoiceRuntime {
    phase: Mutex<VoicePhase>,
    current_session_id: Mutex<Option<String>>,
    captured_pcm: Mutex<Vec<u8>>,
    next_event_id: AtomicU64,
    next_session_id: AtomicU64,
    audio_frames: AtomicU64,
    audio_bytes: AtomicU64,
    wake_count: AtomicU64,
    transcript_count: AtomicU64,
    last_event_ms: AtomicI64,
}

/// 运行时快照。
#[derive(Debug, serde::Serialize)]
pub struct RuntimeSnapshot {
    pub state: &'static str,
    pub current_session_id: Option<String>,
    pub audio_frames: u64,
    pub audio_bytes: u64,
    pub wake_count: u64,
    pub transcript_count: u64,
    pub last_event_ms: Option<i64>,
}

impl VoiceRuntime {
    pub fn new() -> Self {
        Self {
            phase: Mutex::new(VoicePhase::Idle),
            current_session_id: Mutex::new(None),
            captured_pcm: Mutex::new(Vec::new()),
            next_event_id: AtomicU64::new(1),
            next_session_id: AtomicU64::new(1),
            audio_frames: AtomicU64::new(0),
            audio_bytes: AtomicU64::new(0),
            wake_count: AtomicU64::new(0),
            transcript_count: AtomicU64::new(0),
            last_event_ms: AtomicI64::new(0),
        }
    }

    pub async fn snapshot(&self) -> RuntimeSnapshot {
        let phase = *self.phase.lock().await;
        let current_session_id = self.current_session_id.lock().await.clone();
        let last_event_ms = self.last_event_ms.load(Ordering::Relaxed);
        RuntimeSnapshot {
            state: phase.as_str(),
            current_session_id,
            audio_frames: self.audio_frames.load(Ordering::Relaxed),
            audio_bytes: self.audio_bytes.load(Ordering::Relaxed),
            wake_count: self.wake_count.load(Ordering::Relaxed),
            transcript_count: self.transcript_count.load(Ordering::Relaxed),
            last_event_ms: (last_event_ms > 0).then_some(last_event_ms),
        }
    }

    pub async fn set_phase(&self, phase: VoicePhase) {
        *self.phase.lock().await = phase;
    }

    pub async fn current_phase(&self) -> VoicePhase {
        *self.phase.lock().await
    }

    pub async fn reset(&self) {
        *self.phase.lock().await = VoicePhase::Idle;
        *self.current_session_id.lock().await = None;
        self.captured_pcm.lock().await.clear();
        self.audio_frames.store(0, Ordering::Relaxed);
        self.audio_bytes.store(0, Ordering::Relaxed);
    }

    pub async fn start_session(&self) -> String {
        let seq = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("voice-{}-{seq}", now_ms());
        *self.current_session_id.lock().await = Some(session_id.clone());
        session_id
    }

    pub async fn session_id(&self) -> Option<String> {
        self.current_session_id.lock().await.clone()
    }

    pub async fn append_captured_pcm(&self, pcm: &[u8]) {
        self.captured_pcm.lock().await.extend_from_slice(pcm);
    }

    pub async fn take_captured_pcm(&self) -> Vec<u8> {
        std::mem::take(&mut *self.captured_pcm.lock().await)
    }

    pub fn next_event_id(&self) -> u64 {
        self.next_event_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn mark_event(&self) {
        self.last_event_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn inc_audio(&self, frames: u64, bytes: u64) {
        self.audio_frames.fetch_add(frames, Ordering::Relaxed);
        self.audio_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn inc_wake(&self) {
        self.wake_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_transcript(&self) {
        self.transcript_count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for VoiceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 创建基础事件模板。
pub fn make_event(
    runtime: &VoiceRuntime,
    event_type: &str,
    session_id: Option<String>,
    phase: VoicePhase,
) -> VoiceEvent {
    VoiceEvent {
        id: runtime.next_event_id(),
        event_type: event_type.to_string(),
        timestamp_ms: now_ms(),
        session_id,
        state: phase.as_str().to_string(),
        keyword: None,
        transcript: None,
        assistant_text: None,
        media: None,
        file_path: None,
        confidence: None,
        final_result: false,
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_names_are_stable() {
        assert_eq!(VoicePhase::Idle.as_str(), "idle");
        assert_eq!(VoicePhase::WakeDetected.as_str(), "wake_detected");
        assert_eq!(VoicePhase::Completed.as_str(), "completed");
    }

    #[tokio::test]
    async fn session_id_increments() {
        let rt = VoiceRuntime::new();
        let s1 = rt.start_session().await;
        let s2 = rt.start_session().await;
        assert_ne!(s1, s2);
    }

    #[tokio::test]
    async fn captured_pcm_works() {
        let rt = VoiceRuntime::new();
        rt.append_captured_pcm(&[1, 2, 3]).await;
        rt.append_captured_pcm(&[4, 5]).await;
        let data = rt.take_captured_pcm().await;
        assert_eq!(data, vec![1, 2, 3, 4, 5]);
        // 二次取为空
        assert!(rt.take_captured_pcm().await.is_empty());
    }
}
