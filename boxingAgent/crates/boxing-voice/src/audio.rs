//! PCM 音频格式工具。

/// 将 little-endian i16 PCM 转为 f32 归一化样本（[-1.0, 1.0]）。
///
/// 使用 32768.0 作为除数（非 i16::MAX=32767），使 -32768 精确映射到 -1.0。
pub fn pcm_i16_to_f32(pcm_le_i16: &[u8]) -> anyhow::Result<Vec<f32>> {
    if pcm_le_i16.len() % 2 != 0 {
        anyhow::bail!("PCM data has odd byte length");
    }
    Ok(pcm_le_i16
        .chunks_exact(2)
        .map(|bytes| {
            let raw = i16::from_le_bytes([bytes[0], bytes[1]]);
            raw as f32 / 32768.0
        })
        .collect())
}

/// 从 f32 样本转回 little-endian i16 PCM 字节。
pub fn pcm_f32_to_i16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * 32768.0) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// 读取 WAV 文件头并提取 PCM 数据（仅支持 16kHz mono 16-bit）。
pub fn extract_pcm_from_wav(wav_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, u32)> {
    if wav_bytes.len() < 44 {
        anyhow::bail!("WAV too short (need at least 44-byte header)");
    }

    // RIFF header check
    if &wav_bytes[0..4] != b"RIFF" || &wav_bytes[8..12] != b"WAVE" {
        anyhow::bail!("not a valid WAV file");
    }

    // 找到 data chunk
    let mut offset = 12usize;
    let mut sample_rate = 16000u32;
    while offset + 8 <= wav_bytes.len() {
        let chunk_id = &wav_bytes[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            wav_bytes[offset + 4],
            wav_bytes[offset + 5],
            wav_bytes[offset + 6],
            wav_bytes[offset + 7],
        ]) as usize;

        match chunk_id {
            b"fmt " => {
                if offset + 24 <= wav_bytes.len() {
                    sample_rate = u32::from_le_bytes([
                        wav_bytes[offset + 12],
                        wav_bytes[offset + 13],
                        wav_bytes[offset + 14],
                        wav_bytes[offset + 15],
                    ]);
                }
            }
            b"data" => {
                let start = offset + 8;
                let end = (start + chunk_size).min(wav_bytes.len());
                return Ok((wav_bytes[start..end].to_vec(), sample_rate));
            }
            _ => {}
        }
        offset += 8 + chunk_size;
        if chunk_size % 2 != 0 {
            offset += 1; // padding
        }
    }

    anyhow::bail!("WAV data chunk not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_roundtrip() {
        let original: Vec<i16> = vec![0, 100, -1000, 32767, -32768];
        let bytes: Vec<u8> = original.iter().flat_map(|v| v.to_le_bytes()).collect();
        let floats = pcm_i16_to_f32(&bytes).unwrap();
        let back = pcm_f32_to_i16(&floats);
        assert_eq!(back, bytes);
    }

    #[test]
    fn rejects_odd_length() {
        assert!(pcm_i16_to_f32(&[0, 1, 2]).is_err());
    }

    #[test]
    fn clamps_to_range() {
        let floats = vec![-2.0, 2.0];
        let bytes = pcm_f32_to_i16(&floats);
        assert_eq!(bytes, vec![0, 128, 255, 127]);
    }
}
