use super::{TtsConfig, TtsProvider, TtsProviderError};

/// Orpheus TTS provider — calls a local Orpheus server via the OpenAI-compatible
/// `/v1/audio/speech` endpoint (the same endpoint llama-server exposes).
pub struct OrpheusProvider {
    base_url: String,
    model: String,
    default_voice: String,
    available: bool,
    mock: bool,
}

impl OrpheusProvider {
    pub fn new(base_url: &str, model: &str, default_voice: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            default_voice: default_voice.to_string(),
            available: true,
            mock: false,
        }
    }

    /// Create a mock provider for testing (no HTTP calls).
    pub fn mock() -> Self {
        Self {
            base_url: String::new(),
            model: "orpheus-mock".to_string(),
            default_voice: "tara".to_string(),
            available: true,
            mock: true,
        }
    }

    /// Set availability (useful for testing failover).
    pub fn set_available(&mut self, available: bool) {
        self.available = available;
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn mock_wav_audio() -> Vec<u8> {
        let mut wav = vec![0u8; 44];
        wav[..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        wav[12..16].copy_from_slice(b"fmt ");
        wav
    }
}

impl TtsProvider for OrpheusProvider {
    fn name(&self) -> &str {
        "orpheus"
    }

    fn synthesize(&self, text: &str, config: &TtsConfig) -> Result<Vec<u8>, TtsProviderError> {
        if self.mock {
            return Ok(Self::mock_wav_audio());
        }

        // Build the request body matching the OpenAI TTS API format.
        let voice = if config.voice.is_empty() {
            &self.default_voice
        } else {
            &config.voice
        };

        let body = serde_json::json!({
            "model": self.model,
            "input": text,
            "voice": voice,
            "speed": config.speed,
            "response_format": config.format.as_str(),
        });

        // Synchronous HTTP call via reqwest::blocking.
        let url = format!("{}/v1/audio/speech", self.base_url);
        let resp = reqwest::blocking::Client::new()
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| TtsProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().unwrap_or_default();
            return Err(TtsProviderError::SynthesisFailed(format!(
                "Orpheus returned {status}: {body_text}"
            )));
        }

        let bytes = resp
            .bytes()
            .map_err(|e| TtsProviderError::Http(e.to_string()))?;
        Ok(bytes.to_vec())
    }

    fn supported_voices(&self) -> Vec<String> {
        // Standard Orpheus voice set.
        vec![
            "tara".to_string(),
            "leah".to_string(),
            "jess".to_string(),
            "leo".to_string(),
            "dan".to_string(),
            "mia".to_string(),
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
        let provider = OrpheusProvider::mock();
        let config = TtsConfig::default();
        let audio = provider.synthesize("Hello", &config).unwrap();
        assert_eq!(&audio[..4], b"RIFF");
    }

    #[test]
    fn test_name() {
        let provider = OrpheusProvider::mock();
        assert_eq!(provider.name(), "orpheus");
    }

    #[test]
    fn test_supported_voices() {
        let provider = OrpheusProvider::mock();
        let voices = provider.supported_voices();
        assert!(voices.contains(&"tara".to_string()));
        assert!(voices.len() >= 4);
    }

    #[test]
    fn test_availability_toggle() {
        let mut provider = OrpheusProvider::mock();
        assert!(provider.is_available());
        provider.set_available(false);
        assert!(!provider.is_available());
    }

    #[test]
    fn test_constructor() {
        let provider = OrpheusProvider::new("http://localhost:8080", "orpheus", "tara");
        assert_eq!(provider.base_url(), "http://localhost:8080");
        assert_eq!(provider.model(), "orpheus");
        assert!(provider.is_available());
    }
}
