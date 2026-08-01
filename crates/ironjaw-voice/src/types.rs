use serde::{Deserialize, Serialize};

/// Supported audio formats for voice pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Pcm16,
    Wav,
    Mp3,
    Opus,
}

impl AudioFormat {
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Pcm16 => "audio/pcm",
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
            Self::Opus => "audio/opus",
        }
    }

    pub fn file_extension(&self) -> &'static str {
        match self {
            Self::Pcm16 => "pcm",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
        }
    }

    /// Detect audio format from magic bytes.
    pub fn detect(data: &[u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }
        // WAV: "RIFF"
        if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WAVE" {
            return Some(Self::Wav);
        }
        // MP3: starts with 0xFF 0xFB/0xF3/0xF2 or ID3 tag
        if (data[0] == 0xFF && (data[1] & 0xE0) == 0xE0) || data.starts_with(b"ID3") {
            return Some(Self::Mp3);
        }
        // Opus: OggS header with Opus ident
        if data.starts_with(b"OggS") {
            return Some(Self::Opus);
        }
        None
    }
}

/// A chunk of audio data.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub data: Vec<u8>,
    pub format: AudioFormat,
    pub sample_rate: u32,
}

/// Result of speech-to-text transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResult {
    pub text: String,
    pub language: Option<String>,
    pub duration_secs: f32,
}

/// Request for text-to-speech synthesis.
#[derive(Debug, Clone, Serialize)]
pub struct SpeechRequest {
    pub model: String,
    pub input: String,
    pub voice: String,
}

/// Voice session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceSessionState {
    Idle,
    Listening,
    Processing,
    Speaking,
}

/// Voice mode: push-to-talk or voice-activity-detection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    #[default]
    PushToTalk,
    Vad,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_format_mime_types() {
        assert_eq!(AudioFormat::Pcm16.mime_type(), "audio/pcm");
        assert_eq!(AudioFormat::Wav.mime_type(), "audio/wav");
        assert_eq!(AudioFormat::Mp3.mime_type(), "audio/mpeg");
        assert_eq!(AudioFormat::Opus.mime_type(), "audio/opus");
    }

    #[test]
    fn test_detect_wav() {
        let mut data = vec![0u8; 12];
        data[..4].copy_from_slice(b"RIFF");
        data[8..12].copy_from_slice(b"WAVE");
        assert_eq!(AudioFormat::detect(&data), Some(AudioFormat::Wav));
    }

    #[test]
    fn test_detect_mp3_sync() {
        let data = vec![0xFF, 0xFB, 0x90, 0x00];
        assert_eq!(AudioFormat::detect(&data), Some(AudioFormat::Mp3));
    }

    #[test]
    fn test_detect_mp3_id3() {
        let data = b"ID3\x04\x00\x00\x00\x00\x00\x00";
        assert_eq!(AudioFormat::detect(data), Some(AudioFormat::Mp3));
    }

    #[test]
    fn test_detect_opus() {
        let data = b"OggS\x00\x02\x00\x00";
        assert_eq!(AudioFormat::detect(data), Some(AudioFormat::Opus));
    }

    #[test]
    fn test_detect_unknown() {
        assert_eq!(AudioFormat::detect(&[0, 0, 0, 0]), None);
        assert_eq!(AudioFormat::detect(&[1, 2]), None);
    }

    #[test]
    fn test_voice_mode_default() {
        assert_eq!(VoiceMode::default(), VoiceMode::PushToTalk);
    }
}
