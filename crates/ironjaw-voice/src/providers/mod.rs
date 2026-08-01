pub mod coqui;
pub mod orpheus;
pub mod piper;

use std::collections::HashMap;
use thiserror::Error;

use crate::types::AudioFormat;

/// Configuration for a TTS synthesis request.
#[derive(Debug, Clone)]
pub struct TtsConfig {
    /// Voice identifier (provider-specific).
    pub voice: String,
    /// Playback speed multiplier (1.0 = normal).
    pub speed: f32,
    /// Desired output audio format.
    pub format: TtsOutputFormat,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            voice: String::new(),
            speed: 1.0,
            format: TtsOutputFormat::Wav,
        }
    }
}

/// Output format for TTS synthesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsOutputFormat {
    Wav,
    Mp3,
    Ogg,
}

impl TtsOutputFormat {
    pub fn as_audio_format(&self) -> AudioFormat {
        match self {
            Self::Wav => AudioFormat::Wav,
            Self::Mp3 => AudioFormat::Mp3,
            Self::Ogg => AudioFormat::Opus,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
        }
    }
}

/// Errors from the TTS provider system.
#[derive(Debug, Error)]
pub enum TtsProviderError {
    #[error("Provider not found: {0}")]
    ProviderNotFound(String),
    #[error("Provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("Synthesis failed: {0}")]
    SynthesisFailed(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("No default provider configured")]
    NoDefaultProvider,
}

/// Trait for pluggable TTS providers.
pub trait TtsProvider: Send + Sync {
    /// Provider name (e.g., "orpheus", "piper", "coqui").
    fn name(&self) -> &str;

    /// Synthesize text to audio bytes.
    fn synthesize(&self, text: &str, config: &TtsConfig) -> Result<Vec<u8>, TtsProviderError>;

    /// List voices supported by this provider.
    fn supported_voices(&self) -> Vec<String>;

    /// Check if the provider is currently available.
    fn is_available(&self) -> bool;
}

/// Registry of TTS providers with routing and default fallback.
pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn TtsProvider>>,
    default_provider: Option<String>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider: None,
        }
    }

    /// Register a provider. The first registered provider becomes the default.
    pub fn register(&mut self, provider: Box<dyn TtsProvider>) {
        let name = provider.name().to_string();
        if self.default_provider.is_none() {
            self.default_provider = Some(name.clone());
        }
        self.providers.insert(name, provider);
    }

    /// Set which provider is the default.
    pub fn set_default(&mut self, name: &str) -> Result<(), TtsProviderError> {
        if !self.providers.contains_key(name) {
            return Err(TtsProviderError::ProviderNotFound(name.to_string()));
        }
        self.default_provider = Some(name.to_string());
        Ok(())
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<&dyn TtsProvider> {
        self.providers.get(name).map(|p| p.as_ref())
    }

    /// Get the default provider name.
    pub fn default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    /// List all registered provider names.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Number of registered providers.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Synthesize text using a named provider (or the default).
    pub fn synthesize(
        &self,
        text: &str,
        provider_name: Option<&str>,
        config: &TtsConfig,
    ) -> Result<Vec<u8>, TtsProviderError> {
        let name = match provider_name {
            Some(n) => n,
            None => self
                .default_provider
                .as_deref()
                .ok_or(TtsProviderError::NoDefaultProvider)?,
        };

        let provider = self
            .providers
            .get(name)
            .ok_or_else(|| TtsProviderError::ProviderNotFound(name.to_string()))?;

        if !provider.is_available() {
            return Err(TtsProviderError::ProviderUnavailable(name.to_string()));
        }

        provider.synthesize(text, config)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock provider for testing the registry.
    struct MockProvider {
        provider_name: String,
        available: bool,
        voices: Vec<String>,
    }

    impl MockProvider {
        fn new(name: &str, available: bool) -> Self {
            Self {
                provider_name: name.to_string(),
                available,
                voices: vec!["default".to_string()],
            }
        }

        fn with_voices(mut self, voices: Vec<String>) -> Self {
            self.voices = voices;
            self
        }
    }

    impl TtsProvider for MockProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn synthesize(&self, text: &str, _config: &TtsConfig) -> Result<Vec<u8>, TtsProviderError> {
            if !self.available {
                return Err(TtsProviderError::ProviderUnavailable(
                    self.provider_name.clone(),
                ));
            }
            // Return text bytes as "audio" for testing
            Ok(text.as_bytes().to_vec())
        }

        fn supported_voices(&self) -> Vec<String> {
            self.voices.clone()
        }

        fn is_available(&self) -> bool {
            self.available
        }
    }

    #[test]
    fn test_empty_registry() {
        let registry = ProviderRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.default_provider().is_none());
    }

    #[test]
    fn test_register_sets_default() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("alpha", true)));
        assert_eq!(registry.default_provider(), Some("alpha"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_first_registered_is_default() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("first", true)));
        registry.register(Box::new(MockProvider::new("second", true)));
        assert_eq!(registry.default_provider(), Some("first"));
    }

    #[test]
    fn test_set_default() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("alpha", true)));
        registry.register(Box::new(MockProvider::new("beta", true)));
        registry.set_default("beta").unwrap();
        assert_eq!(registry.default_provider(), Some("beta"));
    }

    #[test]
    fn test_set_default_nonexistent() {
        let mut registry = ProviderRegistry::new();
        let err = registry.set_default("ghost").unwrap_err();
        assert!(matches!(err, TtsProviderError::ProviderNotFound(_)));
    }

    #[test]
    fn test_get_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("alpha", true)));
        assert!(registry.get("alpha").is_some());
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn test_synthesize_with_default() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("alpha", true)));
        let config = TtsConfig::default();
        let result = registry.synthesize("hello", None, &config).unwrap();
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_synthesize_with_named_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("alpha", true)));
        registry.register(Box::new(MockProvider::new("beta", true)));
        let config = TtsConfig::default();
        let result = registry.synthesize("test", Some("beta"), &config).unwrap();
        assert_eq!(result, b"test");
    }

    #[test]
    fn test_synthesize_provider_not_found() {
        let registry = ProviderRegistry::new();
        let config = TtsConfig::default();
        let err = registry
            .synthesize("text", Some("ghost"), &config)
            .unwrap_err();
        assert!(matches!(err, TtsProviderError::ProviderNotFound(_)));
    }

    #[test]
    fn test_synthesize_no_default() {
        let registry = ProviderRegistry::new();
        let config = TtsConfig::default();
        let err = registry.synthesize("text", None, &config).unwrap_err();
        assert!(matches!(err, TtsProviderError::NoDefaultProvider));
    }

    #[test]
    fn test_synthesize_unavailable_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("down", false)));
        let config = TtsConfig::default();
        let err = registry.synthesize("text", None, &config).unwrap_err();
        assert!(matches!(err, TtsProviderError::ProviderUnavailable(_)));
    }

    #[test]
    fn test_provider_names() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new("alpha", true)));
        registry.register(Box::new(MockProvider::new("beta", true)));
        let mut names = registry.provider_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_provider_voices() {
        let mut registry = ProviderRegistry::new();
        let provider =
            MockProvider::new("multi", true).with_voices(vec!["voice-a".into(), "voice-b".into()]);
        registry.register(Box::new(provider));
        let p = registry.get("multi").unwrap();
        assert_eq!(p.supported_voices(), vec!["voice-a", "voice-b"]);
    }

    #[test]
    fn test_tts_config_defaults() {
        let config = TtsConfig::default();
        assert!(config.voice.is_empty());
        assert!((config.speed - 1.0).abs() < f32::EPSILON);
        assert_eq!(config.format, TtsOutputFormat::Wav);
    }

    #[test]
    fn test_tts_output_format_as_str() {
        assert_eq!(TtsOutputFormat::Wav.as_str(), "wav");
        assert_eq!(TtsOutputFormat::Mp3.as_str(), "mp3");
        assert_eq!(TtsOutputFormat::Ogg.as_str(), "ogg");
    }

    #[test]
    fn test_tts_output_format_to_audio_format() {
        assert_eq!(TtsOutputFormat::Wav.as_audio_format(), AudioFormat::Wav);
        assert_eq!(TtsOutputFormat::Mp3.as_audio_format(), AudioFormat::Mp3);
        assert_eq!(TtsOutputFormat::Ogg.as_audio_format(), AudioFormat::Opus);
    }
}
