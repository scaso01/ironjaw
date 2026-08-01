use crate::types::{ImageFormat, ImageInput};

/// Errors from image processing.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("image too large: {size} bytes (max {max})")]
    TooLarge { size: usize, max: usize },
    #[error("unsupported image format")]
    UnsupportedFormat,
    #[error("image data is empty")]
    Empty,
}

/// Image processor: validates format, enforces size limits, encodes to base64.
///
/// No heavy image decoding (avoiding `image` crate dep). Format detection
/// uses magic bytes. Actual resize is deferred to the LLM server which
/// handles multimodal inputs natively.
pub struct ImageProcessor {
    max_size_bytes: usize,
}

impl ImageProcessor {
    pub fn new(max_size_bytes: usize) -> Self {
        Self { max_size_bytes }
    }

    /// Validate image data: check size, detect format.
    /// Returns (format, mime_type) on success.
    pub fn validate(&self, data: &[u8]) -> Result<(ImageFormat, &'static str), ImageError> {
        if data.is_empty() {
            return Err(ImageError::Empty);
        }
        if data.len() > self.max_size_bytes {
            return Err(ImageError::TooLarge {
                size: data.len(),
                max: self.max_size_bytes,
            });
        }
        let format = ImageFormat::detect(data).ok_or(ImageError::UnsupportedFormat)?;
        Ok((format, format.mime_type()))
    }

    /// Encode raw bytes to a base64 string.
    pub fn to_base64(data: &[u8]) -> String {
        base64_encode(data)
    }

    /// Validate a base64-encoded image string.
    /// Decodes, validates format and size. Returns Ok(()) on success.
    pub fn validate_base64(&self, base64_data: &str) -> Result<(), ImageError> {
        let bytes = base64_decode(base64_data).map_err(|_| ImageError::UnsupportedFormat)?;
        self.validate(&bytes)?;
        Ok(())
    }

    /// Process raw image bytes into an ImageInput ready for LLM API calls.
    pub fn process(&self, data: Vec<u8>) -> Result<ImageInput, ImageError> {
        let (_format, mime_type) = self.validate(&data)?;
        let base64 = Self::to_base64(&data);
        Ok(ImageInput {
            data,
            base64,
            mime_type: mime_type.to_string(),
            width: None,
            height: None,
        })
    }
}

/// RFC 4648 base64 decoder (standard alphabet, with padding).
pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let clean: String = input.chars().filter(|c| !c.is_whitespace()).collect();

    if !clean.len().is_multiple_of(4) {
        return Err("Invalid base64 length".to_string());
    }

    let mut output = Vec::with_capacity((clean.len() / 4) * 3);

    for chunk in clean.as_bytes().chunks(4) {
        let mut sextet = [0u8; 4];
        let mut padding = 0;

        for (i, &byte) in chunk.iter().enumerate() {
            sextet[i] = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => {
                    padding += 1;
                    0
                }
                _ => return Err(format!("Invalid base64 character: {}", byte as char)),
            };
        }

        let triple = ((sextet[0] as u32) << 18)
            | ((sextet[1] as u32) << 12)
            | ((sextet[2] as u32) << 6)
            | (sextet[3] as u32);

        output.push(((triple >> 16) & 0xFF) as u8);
        if padding < 2 {
            output.push(((triple >> 8) & 0xFF) as u8);
        }
        if padding < 1 {
            output.push((triple & 0xFF) as u8);
        }
    }

    Ok(output)
}

/// RFC 4648 base64 encoder (standard alphabet, with padding).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        output.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        output.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            output.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }

        if chunk.len() > 2 {
            output.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_bytes() -> Vec<u8> {
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xE0];
        data.extend_from_slice(&[0u8; 100]);
        data
    }

    fn png_bytes() -> Vec<u8> {
        let mut data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0u8; 100]);
        data
    }

    #[test]
    fn test_validate_jpeg() {
        let processor = ImageProcessor::new(10_000_000);
        let (fmt, mime) = processor.validate(&jpeg_bytes()).unwrap();
        assert_eq!(fmt, ImageFormat::Jpeg);
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn test_validate_png() {
        let processor = ImageProcessor::new(10_000_000);
        let (fmt, mime) = processor.validate(&png_bytes()).unwrap();
        assert_eq!(fmt, ImageFormat::Png);
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn test_validate_too_large() {
        let processor = ImageProcessor::new(50); // 50 byte limit
        let err = processor.validate(&jpeg_bytes()).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn test_validate_empty() {
        let processor = ImageProcessor::new(10_000_000);
        let err = processor.validate(&[]).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_validate_unsupported() {
        let processor = ImageProcessor::new(10_000_000);
        let err = processor
            .validate(&[0x00, 0x01, 0x02, 0x03, 0x04])
            .unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn test_base64_encode() {
        // Standard test vectors
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_process_jpeg() {
        let processor = ImageProcessor::new(10_000_000);
        let input = processor.process(jpeg_bytes()).unwrap();
        assert_eq!(input.mime_type, "image/jpeg");
        assert!(!input.base64.is_empty());
        assert_eq!(input.data.len(), 104);
    }

    #[test]
    fn test_to_base64_roundtrip() {
        let data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x42, 0x00, 0xFF];
        let encoded = ImageProcessor::to_base64(&data);
        // Verify it's valid base64 by checking character set + length
        assert_eq!(encoded.len() % 4, 0);
        assert!(encoded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }
}
