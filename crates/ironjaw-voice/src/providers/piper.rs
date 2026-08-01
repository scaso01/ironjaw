use super::{TtsConfig, TtsProvider, TtsProviderError};

/// Piper TTS provider — calls a local Piper TTS server over HTTP.
/// Default endpoint: `http://127.0.0.1:5000/api/tts`
pub struct PiperProvider {
    endpoint: String,
    available: bool,
    mock: bool,
}

impl PiperProvider {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            available: true,
            mock: false,
        }
    }

    /// Create with the default Piper endpoint.
    pub fn default_endpoint() -> Self {
        Self::new("http://127.0.0.1:5000/api/tts")
    }

    /// Create a mock provider for testing.
    pub fn mock() -> Self {
        Self {
            endpoint: String::new(),
            available: true,
            mock: true,
        }
    }

    pub fn set_available(&mut self, available: bool) {
        self.available = available;
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn mock_wav_audio() -> Vec<u8> {
        let mut wav = vec![0u8; 44];
        wav[..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        wav[12..16].copy_from_slice(b"fmt ");
        // Mark as Piper output for testing
        wav[40..44].copy_from_slice(b"pipr");
        wav
    }
}

impl TtsProvider for PiperProvider {
    fn name(&self) -> &str {
        "piper"
    }

    fn synthesize(&self, text: &str, config: &TtsConfig) -> Result<Vec<u8>, TtsProviderError> {
        if self.mock {
            return Ok(Self::mock_wav_audio());
        }

        // Piper HTTP API: GET/POST with query parameters.
        let mut url = self.endpoint.clone();
        let voice_param = if config.voice.is_empty() {
            String::new()
        } else {
            format!("&voice={}", config.voice)
        };
        url = format!("{url}?text={}{voice_param}", urlencoded(text));

        let resp = reqwest::blocking::Client::new()
            .get(&url)
            .send()
            .map_err(|e| TtsProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(TtsProviderError::SynthesisFailed(format!(
                "Piper returned {status}: {body}"
            )));
        }

        let bytes = resp
            .bytes()
            .map_err(|e| TtsProviderError::Http(e.to_string()))?;
        Ok(bytes.to_vec())
    }

    fn supported_voices(&self) -> Vec<String> {
        // Piper supports many voices; these are common English ones.
        vec![
            "en_US-lessac-medium".to_string(),
            "en_US-amy-medium".to_string(),
            "en_US-ryan-medium".to_string(),
            "en_GB-alan-medium".to_string(),
        ]
    }

    fn is_available(&self) -> bool {
        self.available
    }
}

/// Minimal percent-encoding for URL query values.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0xf) as usize]));
            }
        }
    }
    out
}

const HEX: [u8; 16] = *b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_synthesize() {
        let provider = PiperProvider::mock();
        let config = TtsConfig::default();
        let audio = provider.synthesize("Hello", &config).unwrap();
        assert_eq!(&audio[..4], b"RIFF");
        // Piper mock marker
        assert_eq!(&audio[40..44], b"pipr");
    }

    #[test]
    fn test_name() {
        let provider = PiperProvider::mock();
        assert_eq!(provider.name(), "piper");
    }

    #[test]
    fn test_supported_voices() {
        let provider = PiperProvider::mock();
        let voices = provider.supported_voices();
        assert!(!voices.is_empty());
        assert!(voices.iter().any(|v| v.contains("lessac")));
    }

    #[test]
    fn test_default_endpoint() {
        let provider = PiperProvider::default_endpoint();
        assert_eq!(provider.endpoint(), "http://127.0.0.1:5000/api/tts");
    }

    #[test]
    fn test_availability_toggle() {
        let mut provider = PiperProvider::mock();
        assert!(provider.is_available());
        provider.set_available(false);
        assert!(!provider.is_available());
    }

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencoded("simple"), "simple");
    }
}
