use crate::session::{VoiceSession, VoiceSessionError};
use crate::stt::SttClient;
use crate::tts::TtsClient;
use crate::types::{AudioFormat, TranscriptionResult, VoiceMode};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Configuration for the voice pipeline.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_stt_url")]
    pub stt_url: String,
    #[serde(default = "default_stt_model")]
    pub stt_model: String,
    #[serde(default = "default_tts_url")]
    pub tts_url: String,
    #[serde(default = "default_tts_model")]
    pub tts_model: String,
    #[serde(default = "default_tts_voice")]
    pub tts_voice: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_max_audio_buffer")]
    pub max_audio_buffer_bytes: usize,
    #[serde(default = "default_audio_timeout")]
    pub inactivity_timeout_secs: u64,
}

fn default_stt_url() -> String {
    "http://localhost:8080".to_string()
}
fn default_stt_model() -> String {
    "whisper-large-v3-turbo".to_string()
}
fn default_tts_url() -> String {
    "http://localhost:8080".to_string()
}
fn default_tts_model() -> String {
    "orpheus".to_string()
}
fn default_tts_voice() -> String {
    "tara".to_string()
}
fn default_sample_rate() -> u32 {
    16000
}
fn default_max_audio_buffer() -> usize {
    16_777_216 // 16 MB
}
fn default_audio_timeout() -> u64 {
    30
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            stt_url: default_stt_url(),
            stt_model: default_stt_model(),
            tts_url: default_tts_url(),
            tts_model: default_tts_model(),
            tts_voice: default_tts_voice(),
            sample_rate: default_sample_rate(),
            max_audio_buffer_bytes: default_max_audio_buffer(),
            inactivity_timeout_secs: default_audio_timeout(),
        }
    }
}

/// Voice engine orchestrator: manages sessions, coordinates STT/TTS.
pub struct VoiceEngine {
    stt: Arc<SttClient>,
    tts: Arc<TtsClient>,
    config: VoiceConfig,
    sessions: RwLock<HashMap<String, VoiceSession>>,
}

impl VoiceEngine {
    pub fn new(stt: SttClient, tts: TtsClient, config: VoiceConfig) -> Self {
        Self {
            stt: Arc::new(stt),
            tts: Arc::new(tts),
            config,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a mock engine with mock STT/TTS clients.
    pub fn mock() -> Self {
        Self::new(SttClient::mock(), TtsClient::mock(), VoiceConfig::default())
    }

    pub fn config(&self) -> &VoiceConfig {
        &self.config
    }

    /// Start a new voice session for a connection.
    pub async fn start_session(
        &self,
        connection_id: &str,
        mode: VoiceMode,
    ) -> Result<String, VoiceEngineError> {
        let session_id = format!("voice-{}", uuid::Uuid::new_v4());
        let mut session = VoiceSession::new(
            session_id.clone(),
            mode,
            self.config.sample_rate,
            self.config.max_audio_buffer_bytes,
            self.config.inactivity_timeout_secs,
        );
        session.start().map_err(VoiceEngineError::Session)?;

        info!(
            session_id = %session_id,
            connection_id = %connection_id,
            mode = ?mode,
            "Voice session started"
        );

        let mut sessions = self.sessions.write().await;
        sessions.insert(connection_id.to_string(), session);
        Ok(session_id)
    }

    /// Feed an audio chunk to the session for a connection.
    pub async fn feed_audio(
        &self,
        connection_id: &str,
        data: &[u8],
    ) -> Result<(), VoiceEngineError> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(connection_id)
            .ok_or(VoiceEngineError::NoSession)?;
        session
            .feed_audio(data)
            .map_err(VoiceEngineError::Session)?;
        Ok(())
    }

    /// Stop a voice session, transcribe the audio, and return the result.
    pub async fn stop_session(
        &self,
        connection_id: &str,
    ) -> Result<TranscriptionResult, VoiceEngineError> {
        let (audio, format, _sample_rate) = {
            let mut sessions = self.sessions.write().await;
            let session = sessions
                .get_mut(connection_id)
                .ok_or(VoiceEngineError::NoSession)?;
            session.stop().map_err(VoiceEngineError::Session)?
        };

        if audio.is_empty() {
            // Remove session and return empty result
            self.sessions.write().await.remove(connection_id);
            return Ok(TranscriptionResult {
                text: String::new(),
                language: None,
                duration_secs: 0.0,
            });
        }

        info!(
            connection_id = %connection_id,
            audio_bytes = audio.len(),
            format = ?format,
            "Transcribing audio"
        );

        let result = self
            .stt
            .transcribe(&audio, format)
            .await
            .map_err(|e| VoiceEngineError::Stt(e.to_string()))?;

        // Clean up session
        self.sessions.write().await.remove(connection_id);

        Ok(result)
    }

    /// Synthesize text to audio bytes via TTS.
    pub async fn speak(&self, text: &str) -> Result<Vec<u8>, VoiceEngineError> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let audio = self
            .tts
            .synthesize(text)
            .await
            .map_err(|e| VoiceEngineError::Tts(e.to_string()))?;
        Ok(audio)
    }

    /// Check if a connection has an active voice session.
    pub async fn has_session(&self, connection_id: &str) -> bool {
        self.sessions.read().await.contains_key(connection_id)
    }

    /// Get the buffer size for a connection's session.
    pub async fn buffer_size(&self, connection_id: &str) -> Option<usize> {
        self.sessions
            .read()
            .await
            .get(connection_id)
            .map(|s| s.buffer_size())
    }

    /// Detect audio format from raw bytes.
    pub fn detect_format(data: &[u8]) -> AudioFormat {
        AudioFormat::detect(data).unwrap_or(AudioFormat::Pcm16)
    }

    /// Check for inactive sessions and clean them up.
    pub async fn cleanup_inactive(&self) -> Vec<String> {
        let mut sessions = self.sessions.write().await;
        let inactive: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.is_inactive())
            .map(|(k, _)| k.clone())
            .collect();

        for key in &inactive {
            warn!(connection_id = %key, "Cleaning up inactive voice session");
            sessions.remove(key);
        }

        inactive
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VoiceEngineError {
    #[error("Voice session error: {0}")]
    Session(#[from] VoiceSessionError),
    #[error("No active voice session for this connection")]
    NoSession,
    #[error("STT error: {0}")]
    Stt(String),
    #[error("TTS error: {0}")]
    Tts(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_start_and_stop_session() {
        let engine = VoiceEngine::mock();
        let session_id = engine
            .start_session("conn-1", VoiceMode::PushToTalk)
            .await
            .unwrap();
        assert!(session_id.starts_with("voice-"));
        assert!(engine.has_session("conn-1").await);

        // Feed some audio
        engine.feed_audio("conn-1", &[0u8; 100]).await.unwrap();
        assert_eq!(engine.buffer_size("conn-1").await, Some(100));

        // Stop and get transcription
        let result = engine.stop_session("conn-1").await.unwrap();
        assert_eq!(result.text, "Mock transcription");
        assert!(!engine.has_session("conn-1").await);
    }

    #[tokio::test]
    async fn test_speak() {
        let engine = VoiceEngine::mock();
        let audio = engine.speak("Hello world").await.unwrap();
        assert!(!audio.is_empty());
        // Mock returns WAV header
        assert_eq!(&audio[..4], b"RIFF");
    }

    #[tokio::test]
    async fn test_speak_empty() {
        let engine = VoiceEngine::mock();
        let audio = engine.speak("").await.unwrap();
        assert!(audio.is_empty());
    }

    #[tokio::test]
    async fn test_no_session_error() {
        let engine = VoiceEngine::mock();
        let err = engine.feed_audio("nonexistent", &[0u8; 10]).await;
        assert!(err.is_err());
        assert!(matches!(err.unwrap_err(), VoiceEngineError::NoSession));
    }

    #[tokio::test]
    async fn test_stop_empty_audio() {
        let engine = VoiceEngine::mock();
        engine
            .start_session("conn-1", VoiceMode::PushToTalk)
            .await
            .unwrap();
        // Stop without feeding any audio
        let result = engine.stop_session("conn-1").await.unwrap();
        assert!(result.text.is_empty());
        assert!(!engine.has_session("conn-1").await);
    }

    #[tokio::test]
    async fn test_detect_format() {
        // Unknown format defaults to PCM16
        assert_eq!(
            VoiceEngine::detect_format(&[0, 0, 0, 0]),
            AudioFormat::Pcm16
        );

        // WAV format
        let mut wav = vec![0u8; 12];
        wav[..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        assert_eq!(VoiceEngine::detect_format(&wav), AudioFormat::Wav);
    }

    #[tokio::test]
    async fn test_multiple_sessions() {
        let engine = VoiceEngine::mock();
        engine
            .start_session("conn-1", VoiceMode::PushToTalk)
            .await
            .unwrap();
        engine
            .start_session("conn-2", VoiceMode::Vad)
            .await
            .unwrap();

        assert!(engine.has_session("conn-1").await);
        assert!(engine.has_session("conn-2").await);

        engine.feed_audio("conn-1", &[1u8; 50]).await.unwrap();
        engine.feed_audio("conn-2", &[2u8; 75]).await.unwrap();

        assert_eq!(engine.buffer_size("conn-1").await, Some(50));
        assert_eq!(engine.buffer_size("conn-2").await, Some(75));
    }

    #[test]
    fn test_config_defaults() {
        let config = VoiceConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.sample_rate, 16000);
        assert_eq!(config.max_audio_buffer_bytes, 16_777_216);
        assert_eq!(config.inactivity_timeout_secs, 30);
        assert_eq!(config.tts_voice, "tara");
    }
}
