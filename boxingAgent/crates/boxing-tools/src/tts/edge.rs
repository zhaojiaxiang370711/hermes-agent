//! edge-tts provider — 通过 edge-tts CLI 合成（免费、无需 API key）。

use std::path::Path;

use crate::ToolError;

/// 用 edge-tts 合成 `text` 到 `out`。`voice` 由调用方从 config 传入。
pub async fn generate(text: &str, out: &Path, voice: &str) -> Result<(), ToolError> {
    let mut cmd = tokio::process::Command::new("edge-tts");
    cmd.arg("--voice")
        .arg(voice)
        .arg("--text")
        .arg(text)
        .arg("--write-media")
        .arg(out)
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
        return Err(ToolError::Other(format!("edge-tts failed: {}", stderr.trim())));
    }
    if !out.exists() {
        return Err(ToolError::Other(
            "edge-tts completed but output file not found".into(),
        ));
    }
    Ok(())
}
