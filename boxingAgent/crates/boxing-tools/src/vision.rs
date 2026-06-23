//! `vision` 工具：图像理解（多模态视觉分析）。
//!
//! 与 Hermes 原版 `tools/vision_tools.py` 对等：
//! - 支持文件路径和 base64 data URL
//! - 向视觉模型发送带图片的多模态消息
//! - 返回模型对图像的文字描述/回答

use serde_json::{json, Value};
use std::path::Path;

use crate::{Tool, ToolError};

/// `vision` 工具：加载图片并用视觉模型分析。
pub struct Vision;

#[async_trait::async_trait]
impl Tool for Vision {
    fn name(&self) -> &str {
        "vision"
    }

    fn schema(&self) -> Value {
        json!({
            "name": "vision",
            "description": "Load an image into the conversation so you can see it. Accepts a local file path or data: URL. When your active model has native vision, the image is attached to your context directly. For non-vision models, falls back to an auxiliary vision model that returns a text description.",
            "parameters": {
                "type": "object",
                "properties": {
                    "image_url": {
                        "type": "string",
                        "description": "Local file path or data: URL (data:image/jpeg;base64,...) to load."
                    },
                    "question": {
                        "type": "string",
                        "description": "Your specific question or request about the image."
                    }
                },
                "required": ["image_url", "question"]
            }
        })
    }

    async fn exec(&self, args: Value) -> Result<String, ToolError> {
        let image_url = args
            .get("image_url")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArg("image_url"))?
            .to_string();
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("描述这张图片的内容")
            .to_string();

        // 转换为 base64 data URL
        let data_url = if image_url.starts_with("data:") {
            image_url
        } else {
            let path = Path::new(&image_url);
            if !path.exists() {
                return Err(ToolError::Other(format!("图片文件不存在: {image_url}")));
            }
            image_to_data_url(path)?
        };

        // 构造多模态消息（后续由 provider 转换为 wire 格式）
        // 这里返回 JSON 描述，由调用方处理实际发送
        Ok(serde_json::json!({
            "image_url": data_url,
            "question": question,
        })
        .to_string())
    }
}

/// 将图片文件转为 data URL (base64)。
fn image_to_data_url(path: &Path) -> Result<String, ToolError> {
    let bytes = std::fs::read(path).map_err(|e| ToolError::Other(format!("读取图片失败: {e}")))?;

    // 检测 MIME 类型
    let mime = detect_mime(&bytes, path)?;

    // 检查文件大小（限制 20MB）
    if bytes.len() > 20 * 1024 * 1024 {
        return Err(ToolError::Other("图片文件超过 20MB 限制".into()));
    }

    // 检查图片尺寸（通过快速读取 header）
    if let Some((width, height)) = fast_image_dimensions(&bytes) {
        if width > 8192 || height > 8192 {
            return Err(ToolError::Other(format!(
                "图片尺寸 {width}x{height} 超过 8192x8192 限制"
            )));
        }
    }

    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// 检测图片 MIME 类型（magic bytes + 文件扩展名）。
fn detect_mime(bytes: &[u8], path: &Path) -> Result<&'static str, ToolError> {
    // Magic bytes 检测
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Ok("image/jpeg");
    }
    if bytes.len() >= 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
    {
        return Ok("image/png");
    }
    if bytes.len() >= 6 && bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 {
        return Ok("image/gif");
    }
    if bytes.len() >= 4
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
    {
        return Ok("image/webp");
    }

    // 扩展名 fallback
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "png" => Ok("image/png"),
        "gif" => Ok("image/gif"),
        "webp" => Ok("image/webp"),
        _ => Err(ToolError::Other(format!(
            "不支持的图片格式: {ext}（仅支持 JPEG/PNG/GIF/WebP）"
        ))),
    }
}

/// 快速读取图片尺寸（JPEG PNG header 解析，不完整解码）。
/// 返回 None 表示无法检测（不报错）。
fn fast_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 8 && bytes[0] == 0x89 && bytes[1] == 0x50 {
        // PNG: width/height 在 IHDR 块（offset 16）
        if bytes.len() >= 24
            && bytes[12] == 0x49
            && bytes[13] == 0x48
            && bytes[14] == 0x44
            && bytes[15] == 0x52
        {
            let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            return Some((w, h));
        }
    }
    None // JPEG/GIF/WebP 需要更复杂的解析，暂时跳过
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn schema_is_valid() {
        let schema = Vision.schema();
        assert_eq!(schema["name"], "vision");
        assert!(schema["parameters"]["properties"]["image_url"].is_object());
        assert!(schema["parameters"]["properties"]["question"].is_object());
    }

    #[tokio::test]
    async fn rejects_missing_image() {
        let result = Vision.exec(json!({"question": "what?"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_nonexistent_file() {
        let result = Vision
            .exec(json!({"image_url": "/nonexistent.jpg", "question": "what?"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn loads_png_to_data_url() {
        // 创建一个最小 PNG 文件（1x1 红色像素）
        let png = create_minimal_png();
        let dir = std::env::temp_dir().join(format!("vision-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.png");
        fs::write(&path, &png).unwrap();

        let result = Vision
            .exec(json!({"image_url": path.to_string_lossy(), "question": "what is this?"}))
            .await
            .unwrap();
        assert!(result.contains("data:image/png;base64,"));
        assert!(result.contains("what is this?"));
    }

    /// 创建一个最小的合法 PNG（1x1 红色像素，手写 header + IDAT）。
    fn create_minimal_png() -> Vec<u8> {
        let png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            // IHDR chunk
            0x00, 0x00, 0x00, 0x0D, // length
            0x49, 0x48, 0x44, 0x52, // type: IHDR
            0x00, 0x00, 0x00, 0x01, // width: 1
            0x00, 0x00, 0x00, 0x01, // height: 1
            0x08, 0x02, // bit depth 8, color type 2 (RGB)
            0x00, 0x00, 0x00, // compression, filter, interlace
            0x90, 0x77, 0x53, 0xDE, // CRC
            // IDAT chunk
            0x00, 0x00, 0x00, 0x0A, // length
            0x49, 0x44, 0x41, 0x54, // type: IDAT
            0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x01, 0x01, 0x01, 0x00, // data
            0x18, 0xDD, 0x8D, 0xB4, // CRC
            // IEND chunk
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        png
    }
}
