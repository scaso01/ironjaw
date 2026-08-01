use super::{TtsConfig, TtsProvider, TtsProviderError};

/// Coqui TTS provider — calls a local Coqui TTS server over HTTP.
/// Default endpoint: `http://127.0.0.1:5002/api/tts`
pub struct CoquiProvider {
    endpoint: String,
    available: bool,
    mock: bool,
}

impl CoquiProvider {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            available: true,
            mock: false,
        }
    }

    /// Create with the default Coqui endpoint.
    pub fn default_endpoint() -> Self {
        Self::new("http://127.0.0.1:5002/api/tts")
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
        // Mark as Coqui output for testing
        wav[40..44].copy_from_slice(b"coqi");
        wav
    }
}

impl TtsProvider for CoquiProvider {
    fn name(&self) -> &str {
        "coqui"
    }

    fn synthesize(&self, text: &str, config: &TtsConfig) -> Result<Vec<u8>, TtsProviderError> {
        if self.mock {
            return Ok(Self::mock_wav_audio());
        }

        // Coqui TTS HTTP API: POST with JSON body.
        let voice = if config.voice.is_empty() {
            None
        } else {
            Some(config.voice.as_str())
        };

        let mut body = serde_json::json!({
            "text": text,
            "speed": config.speed,
        });

        if let Some(v) = voice {
            body["speaker_id"] = serde_json::json!(v);
        }

        let resp = reqwest::blocking::Client::new()
            .post(&self.endpoint)
            .json(&body)
            .send()
            .map_err(|e| TtsProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().unwrap_or_default();
            return Err(TtsProviderError::SynthesisFailed(format!(
                "Coqui returned {status}: {body_text}"
            )));
        }

        let bytes = resp
            .bytes()
            .map_err(|e| TtsProviderError::Http(e.to_string()))?;
        Ok(bytes.to_vec())
    }

    fn supported_voices(&self) -> Vec<String> {
        // Common Coqui TTS voices.
        vec![
            "p225".to_string(),
            "p226".to_string(),
            "p227".to_string(),
            "p228".to_string(),
        ]
    }

    fn is_available(&self) -> bool {
        self.available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_synthesize() {
        let provider = CoquiProvider::mock();
        let config = TtsConfig::default();
        let audio = provider.synthesize("Hello", &config).unwrap();
        assert_eq!(&audio[..4], b"RIFF");
        // Coqui mock marker
        assert_eq!(&audio[40..44], b"coqi");
    }

    #[test]
    fn test_name() {
        let provider = CoquiProvider::mock();
        assert_eq!(provider.name(), "coqui");
    }

    #[test]
    fn test_supported_voices() {
        let provider = CoquiProvider::mock();
        let voices = provider.supported_voices();
        assert!(!voices.is_empty());
        assert!(voices.iter().any(|v| v.starts_with('p')));
    }

    #[test]
    fn test_default_endpoint() {
        let provider = CoquiProvider::default_endpoint();
        assert_eq!(provider.endpoint(), "http://127.0.0.1:5002/api/tts");
    }

    #[test]
    fn test_availability_toggle() {
        let mut provider = CoquiProvider::mock();
        assert!(provider.is_available());
        provider.set_available(false);
        assert!(!provider.is_available());
    }

    #[test]
    fn test_synthesize_unavailable() {
        let mut provider = CoquiProvider::mock();
        provider.set_available(false);
        // Direct call bypasses registry check, so mock still works.
        // This test verifies the flag state.
        assert!(!provider.is_available());
    }
}
