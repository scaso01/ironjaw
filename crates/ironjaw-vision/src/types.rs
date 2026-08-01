use serde::{Deserialize, Serialize};

/// An image input for multimodal LLM requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInput {
    /// Raw image bytes (pre-validated)
    #[serde(skip)]
    pub data: Vec<u8>,
    /// Base64-encoded image data (for serialization/API payloads)
    pub base64: String,
    /// MIME type (e.g. "image/jpeg", "image/png")
    pub mime_type: String,
    /// Image width in pixels (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Image height in pixels (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Supported image formats, detected from magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
    Gif,
}

impl ImageFormat {
    /// Detect image format from the first few bytes (magic bytes).
    pub fn detect(data: &[u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }
        // JPEG: FF D8 FF
        if data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
            return Some(Self::Jpeg);
        }
        // PNG: 89 50 4E 47
        if data[0] == 0x89 && data[1] == 0x50 && data[2] == 0x4E && data[3] == 0x47 {
            return Some(Self::Png);
        }
        // WebP: RIFF....WEBP
        if data.len() >= 12
            && data[0] == b'R'
            && data[1] == b'I'
            && data[2] == b'F'
            && data[3] == b'F'
            && data[8] == b'W'
            && data[9] == b'E'
            && data[10] == b'B'
            && data[11] == b'P'
        {
            return Some(Self::Webp);
        }
        // GIF: GIF87a or GIF89a
        if data[0] == b'G' && data[1] == b'I' && data[2] == b'F' {
            return Some(Self::Gif);
        }
        None
    }

    /// Return the MIME type for this format.
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }
}

/// Configuration for the vision engine.
#[derive(Debug, Clone, Deserialize)]
pub struct VisionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_image_size")]
    pub max_image_size_bytes: usize,
}

fn default_max_image_size() -> usize {
    10_485_760 // 10 MB
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_image_size_bytes: default_max_image_size(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_jpeg() {
        let data = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(ImageFormat::detect(&data), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::Jpeg.mime_type(), "image/jpeg");
    }

    #[test]
    fn test_detect_png() {
        let data = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(ImageFormat::detect(&data), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::Png.mime_type(), "image/png");
    }

    #[test]
    fn test_detect_webp() {
        let mut data = vec![b'R', b'I', b'F', b'F', 0, 0, 0, 0, b'W', b'E', b'B', b'P'];
        assert_eq!(ImageFormat::detect(&data), Some(ImageFormat::Webp));
        assert_eq!(ImageFormat::Webp.mime_type(), "image/webp");
        // Corrupt WEBP signature
        data[8] = b'X';
        assert_eq!(ImageFormat::detect(&data), None);
    }

    #[test]
    fn test_detect_gif() {
        let data = b"GIF89a\x00\x00";
        assert_eq!(ImageFormat::detect(data), Some(ImageFormat::Gif));
        assert_eq!(ImageFormat::Gif.mime_type(), "image/gif");
    }

    #[test]
    fn test_detect_unknown() {
        assert_eq!(ImageFormat::detect(&[0x00, 0x01, 0x02, 0x03]), None);
        assert_eq!(ImageFormat::detect(&[0x00]), None);
        assert_eq!(ImageFormat::detect(&[]), None);
    }

    #[test]
    fn test_config_defaults() {
        let config = VisionConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_image_size_bytes, 10_485_760);
    }
}
