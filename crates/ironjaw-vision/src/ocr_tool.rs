use crate::processor::{base64_decode, ImageProcessor};
use crate::types::VisionConfig;

/// OCR / image analysis tool for the tool registry.
///
/// Validates and processes images, preparing them for multimodal LLM requests.
/// The actual LLM call happens in the chat.send handler when images are attached.
pub struct OcrTool {
    processor: ImageProcessor,
}

impl OcrTool {
    pub fn new(config: &VisionConfig) -> Self {
        Self {
            processor: ImageProcessor::new(config.max_image_size_bytes),
        }
    }

    /// Tool metadata: name.
    pub fn name(&self) -> &'static str {
        "ocr"
    }

    /// Tool metadata: description.
    pub fn description(&self) -> &'static str {
        "Analyze an image using the vision-capable LLM. Provide a base64-encoded image and an optional prompt describing what to analyze."
    }

    /// Tool metadata: JSON schema for parameters.
    pub fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "image_base64": {
                    "type": "string",
                    "description": "Base64-encoded image data"
                },
                "prompt": {
                    "type": "string",
                    "description": "What to analyze in the image (default: 'Describe this image in detail')"
                }
            },
            "required": ["image_base64"]
        })
    }

    /// Validate and process the image from base64 input.
    /// Returns the processed ImageInput or an error message.
    pub fn validate_image(&self, base64_data: &str) -> Result<crate::types::ImageInput, String> {
        let bytes = base64_decode(base64_data).map_err(|e| format!("Invalid base64: {e}"))?;
        self.processor.process(bytes).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_decode_roundtrip() {
        let tests = vec![
            ("", b"" as &[u8]),
            ("Zg==", b"f"),
            ("Zm8=", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg==", b"foob"),
            ("Zm9vYmE=", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
        ];

        for (encoded, expected) in tests {
            let decoded = base64_decode(encoded).unwrap();
            assert_eq!(decoded, expected, "Failed for input: {encoded}");
        }
    }

    #[test]
    fn test_base64_decode_invalid() {
        assert!(base64_decode("!!!").is_err());
        assert!(base64_decode("abc").is_err());
    }

    #[test]
    fn test_ocr_tool_metadata() {
        let config = VisionConfig::default();
        let tool = OcrTool::new(&config);
        assert_eq!(tool.name(), "ocr");
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema["properties"]["image_base64"].is_object());
        assert!(schema["properties"]["prompt"].is_object());
    }

    #[test]
    fn test_validate_image_valid_jpeg() {
        let config = VisionConfig::default();
        let tool = OcrTool::new(&config);

        let mut jpeg_data = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg_data.extend_from_slice(&[0u8; 50]);

        let base64 = ImageProcessor::to_base64(&jpeg_data);
        let result = tool.validate_image(&base64);
        assert!(result.is_ok());
        let input = result.unwrap();
        assert_eq!(input.mime_type, "image/jpeg");
    }

    #[test]
    fn test_validate_image_invalid_base64() {
        let config = VisionConfig::default();
        let tool = OcrTool::new(&config);
        let result = tool.validate_image("not-valid-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_image_unsupported_format() {
        let config = VisionConfig::default();
        let tool = OcrTool::new(&config);

        let data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let base64 = ImageProcessor::to_base64(&data);
        let result = tool.validate_image(&base64);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported"));
    }
}
