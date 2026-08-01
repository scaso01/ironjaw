use crate::types::{AudioFormat, VoiceMode, VoiceSessionState};
use tracing::warn;

/// A voice session tracks audio accumulation and state transitions.
pub struct VoiceSession {
    pub id: String,
    pub state: VoiceSessionState,
    pub mode: VoiceMode,
    audio_buffer: Vec<u8>,
    audio_format: AudioFormat,
    sample_rate: u32,
    max_buffer_bytes: usize,
    last_audio_at: Option<std::time::Instant>,
    inactivity_timeout: std::time::Duration,
}

impl VoiceSession {
    pub fn new(
        id: String,
        mode: VoiceMode,
        sample_rate: u32,
        max_buffer_bytes: usize,
        inactivity_timeout_secs: u64,
    ) -> Self {
        Self {
            id,
            state: VoiceSessionState::Idle,
            mode,
            audio_buffer: Vec::new(),
            audio_format: AudioFormat::Pcm16,
            sample_rate,
            max_buffer_bytes,
            last_audio_at: None,
            inactivity_timeout: std::time::Duration::from_secs(inactivity_timeout_secs),
        }
    }

    /// Start listening for audio.
    pub fn start(&mut self) -> Result<(), VoiceSessionError> {
        if self.state != VoiceSessionState::Idle {
            return Err(VoiceSessionError::InvalidState {
                expected: VoiceSessionState::Idle,
                actual: self.state,
            });
        }
        self.state = VoiceSessionState::Listening;
        self.audio_buffer.clear();
        self.last_audio_at = Some(std::time::Instant::now());
        Ok(())
    }

    /// Feed an audio chunk. Returns error if buffer is full.
    pub fn feed_audio(&mut self, data: &[u8]) -> Result<(), VoiceSessionError> {
        if self.state != VoiceSessionState::Listening {
            return Err(VoiceSessionError::InvalidState {
                expected: VoiceSessionState::Listening,
                actual: self.state,
            });
        }

        if self.audio_buffer.len() + data.len() > self.max_buffer_bytes {
            warn!(
                session_id = %self.id,
                buffer = self.audio_buffer.len(),
                incoming = data.len(),
                max = self.max_buffer_bytes,
                "Audio buffer full"
            );
            return Err(VoiceSessionError::BufferFull);
        }

        self.audio_buffer.extend_from_slice(data);
        self.last_audio_at = Some(std::time::Instant::now());
        Ok(())
    }

    /// Transition to processing state, returning the accumulated audio.
    pub fn stop(&mut self) -> Result<(Vec<u8>, AudioFormat, u32), VoiceSessionError> {
        if self.state != VoiceSessionState::Listening {
            return Err(VoiceSessionError::InvalidState {
                expected: VoiceSessionState::Listening,
                actual: self.state,
            });
        }
        self.state = VoiceSessionState::Processing;
        let buffer = std::mem::take(&mut self.audio_buffer);
        Ok((buffer, self.audio_format, self.sample_rate))
    }

    /// Set state to speaking (TTS playback).
    pub fn set_speaking(&mut self) {
        self.state = VoiceSessionState::Speaking;
    }

    /// Reset to idle.
    pub fn reset(&mut self) {
        self.state = VoiceSessionState::Idle;
        self.audio_buffer.clear();
        self.last_audio_at = None;
    }

    /// Check if the session has timed out due to inactivity.
    pub fn is_inactive(&self) -> bool {
        if self.state != VoiceSessionState::Listening {
            return false;
        }
        match self.last_audio_at {
            Some(last) => last.elapsed() >= self.inactivity_timeout,
            None => false,
        }
    }

    /// Get the current buffer size in bytes.
    pub fn buffer_size(&self) -> usize {
        self.audio_buffer.len()
    }

    /// Set the audio format for incoming data.
    pub fn set_format(&mut self, format: AudioFormat) {
        self.audio_format = format;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VoiceSessionError {
    #[error("Invalid state: expected {expected:?}, got {actual:?}")]
    InvalidState {
        expected: VoiceSessionState,
        actual: VoiceSessionState,
    },
    #[error("Audio buffer full")]
    BufferFull,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> VoiceSession {
        VoiceSession::new(
            "test-session".to_string(),
            VoiceMode::PushToTalk,
            16000,
            1024, // small buffer for testing
            30,
        )
    }

    #[test]
    fn test_lifecycle() {
        let mut session = test_session();
        assert_eq!(session.state, VoiceSessionState::Idle);

        session.start().unwrap();
        assert_eq!(session.state, VoiceSessionState::Listening);

        session.feed_audio(&[0u8; 100]).unwrap();
        assert_eq!(session.buffer_size(), 100);

        let (audio, format, rate) = session.stop().unwrap();
        assert_eq!(session.state, VoiceSessionState::Processing);
        assert_eq!(audio.len(), 100);
        assert_eq!(format, AudioFormat::Pcm16);
        assert_eq!(rate, 16000);

        session.set_speaking();
        assert_eq!(session.state, VoiceSessionState::Speaking);

        session.reset();
        assert_eq!(session.state, VoiceSessionState::Idle);
        assert_eq!(session.buffer_size(), 0);
    }

    #[test]
    fn test_buffer_overflow() {
        let mut session = test_session(); // 1024 max
        session.start().unwrap();
        session.feed_audio(&[0u8; 500]).unwrap();
        session.feed_audio(&[0u8; 500]).unwrap();
        // Next chunk exceeds buffer
        let err = session.feed_audio(&[0u8; 100]).unwrap_err();
        assert!(matches!(err, VoiceSessionError::BufferFull));
    }

    #[test]
    fn test_invalid_state_start() {
        let mut session = test_session();
        session.start().unwrap();
        // Can't start again while listening
        let err = session.start().unwrap_err();
        assert!(matches!(err, VoiceSessionError::InvalidState { .. }));
    }

    #[test]
    fn test_invalid_state_feed() {
        let mut session = test_session();
        // Can't feed while idle
        let err = session.feed_audio(&[0u8; 10]).unwrap_err();
        assert!(matches!(err, VoiceSessionError::InvalidState { .. }));
    }

    #[test]
    fn test_invalid_state_stop() {
        let mut session = test_session();
        // Can't stop while idle
        let err = session.stop().unwrap_err();
        assert!(matches!(err, VoiceSessionError::InvalidState { .. }));
    }

    #[test]
    fn test_inactivity_not_triggered_initially() {
        let session = test_session();
        assert!(!session.is_inactive());
    }

    #[test]
    fn test_set_format() {
        let mut session = test_session();
        session.set_format(AudioFormat::Wav);
        session.start().unwrap();
        session.feed_audio(&[0u8; 10]).unwrap();
        let (_, format, _) = session.stop().unwrap();
        assert_eq!(format, AudioFormat::Wav);
    }
}
