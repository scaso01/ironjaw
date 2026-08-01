use crate::types::SpeechRequest;
use tracing::{error, info};

/// Text-to-speech client for llama-server's OpenAI-compatible speech endpoint.
pub struct TtsClient {
    client: reqwest::Client,
    base_url: String,
    model: String,
    voice: String,
    mock: bool,
}

impl TtsClient {
    pub fn new(base_url: &str, model: &str, voice: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            voice: voice.to_string(),
            mock: false,
        }
    }

    /// Create a mock client that returns empty audio.
    pub fn mock() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: String::new(),
            model: String::new(),
            voice: String::new(),
            mock: true,
        }
    }

    pub fn is_mock(&self) -> bool {
        self.mock
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }

    /// Synthesize text to audio bytes.
    pub async fn synthesize(&self, text: &str) -> anyhow::Result<Vec<u8>> {
        if self.mock {
            // Return a minimal WAV header (44 bytes) as mock audio
            let mut wav = vec![0u8; 44];
            wav[..4].copy_from_slice(b"RIFF");
            wav[8..12].copy_from_slice(b"WAVE");
            wav[12..16].copy_from_slice(b"fmt ");
            return Ok(wav);
        }

        let url = format!("{}/v1/audio/speech", self.base_url);
        let request = SpeechRequest {
            model: self.model.clone(),
            input: text.to_string(),
            voice: self.voice.clone(),
        };

        info!(url = %url, model = %self.model, voice = %self.voice, text_len = text.len(), "TTS request");

        let resp = self.client.post(&url).json(&request).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "TTS request failed");
            return Err(anyhow::anyhow!("TTS failed with status {status}: {body}"));
        }

        let bytes = resp.bytes().await?;
        info!(audio_bytes = bytes.len(), "TTS response received");

        // Convert float32 WAV (format 3) to PCM16 WAV (format 1) for Android compat
        let audio = bytes.to_vec();
        let converted = convert_float32_to_pcm16_wav(&audio).unwrap_or(audio);
        Ok(converted)
    }
}

/// Convert a 32-bit float WAV (format 3) to 16-bit PCM WAV (format 1).
/// Returns None if the input is not a float32 WAV.
fn convert_float32_to_pcm16_wav(data: &[u8]) -> Option<Vec<u8>> {
    // Validate WAV header: need at least 44 bytes, RIFF/WAVE magic
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return None;
    }

    // Parse fmt chunk: format at offset 20 (2 bytes LE)
    let audio_format = u16::from_le_bytes([data[20], data[21]]);
    if audio_format != 3 {
        // Not float format, return as-is
        return None;
    }

    let channels = u16::from_le_bytes([data[22], data[23]]);
    let sample_rate = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let bits_per_sample = u16::from_le_bytes([data[34], data[35]]);

    if bits_per_sample != 32 {
        return None;
    }

    // Find data chunk (skip past fmt chunk which may have extra bytes)
    let mut pos = 12; // after RIFF header + WAVE
    let mut data_start = 0usize;
    let mut data_len = 0usize;
    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        if chunk_id == b"data" {
            data_start = pos + 8;
            data_len = chunk_size;
            break;
        }
        pos += 8 + chunk_size;
        // Word-align
        if pos % 2 != 0 {
            pos += 1;
        }
    }

    if data_start == 0 || data_start + data_len > data.len() {
        return None;
    }

    // Convert float32 samples to PCM16
    let num_samples = data_len / 4;
    let pcm16_data_len = num_samples * 2;
    let mut pcm16_samples = Vec::with_capacity(pcm16_data_len);

    for i in 0..num_samples {
        let offset = data_start + i * 4;
        let sample_f32 = f32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        // Clamp to [-1.0, 1.0] and scale to i16 range
        let clamped = sample_f32.clamp(-1.0, 1.0);
        let sample_i16 = (clamped * 32767.0) as i16;
        pcm16_samples.extend_from_slice(&sample_i16.to_le_bytes());
    }

    // Build new WAV header (44 bytes) + PCM16 data
    let byte_rate = sample_rate * channels as u32 * 2; // 2 bytes per sample
    let block_align = channels * 2;
    let file_size = 36 + pcm16_data_len as u32;

    let mut out = Vec::with_capacity(44 + pcm16_data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm16_data_len as u32).to_le_bytes());
    out.extend_from_slice(&pcm16_samples);

    info!(
        original_bytes = data.len(),
        converted_bytes = out.len(),
        samples = num_samples,
        sample_rate = sample_rate,
        "Converted float32 WAV to PCM16 WAV"
    );

    Some(out)
}

/// A single entry in the character voice roster.
pub struct VoiceEntry {
    pub id: String,
    pub name: String,
    pub character: String,
    pub description: String,
}

/// Available character voices for TTS.
pub struct VoiceRoster {
    voices: Vec<VoiceEntry>,
}

impl VoiceRoster {
    /// Build the default roster of character voices.
    pub fn default_roster() -> Self {
        Self {
            voices: vec![
                VoiceEntry {
                    id: "kitt".into(),
                    name: "KITT".into(),
                    character: "Knight Rider".into(),
                    description: "Calm AI assistant".into(),
                },
                VoiceEntry {
                    id: "drclaw".into(),
                    name: "Dr. Claw".into(),
                    character: "Inspector Gadget".into(),
                    description: "Deep menacing villain".into(),
                },
                VoiceEntry {
                    id: "optimus".into(),
                    name: "Optimus Prime".into(),
                    character: "Transformers".into(),
                    description: "Noble commanding leader".into(),
                },
                VoiceEntry {
                    id: "vader".into(),
                    name: "Darth Vader".into(),
                    character: "Star Wars".into(),
                    description: "Deep resonant Sith Lord".into(),
                },
                VoiceEntry {
                    id: "mrt".into(),
                    name: "Mr. T".into(),
                    character: "A-Team".into(),
                    description: "Tough distinctive".into(),
                },
                VoiceEntry {
                    id: "terminator".into(),
                    name: "Terminator".into(),
                    character: "Terminator".into(),
                    description: "Flat mechanical".into(),
                },
                VoiceEntry {
                    id: "hal9000".into(),
                    name: "HAL 9000".into(),
                    character: "2001: A Space Odyssey".into(),
                    description: "Eerily calm AI".into(),
                },
                VoiceEntry {
                    id: "skeletor".into(),
                    name: "Skeletor".into(),
                    character: "He-Man".into(),
                    description: "Villain cackle".into(),
                },
                VoiceEntry {
                    id: "robocop".into(),
                    name: "RoboCop".into(),
                    character: "RoboCop".into(),
                    description: "Clipped robotic".into(),
                },
                VoiceEntry {
                    id: "maxheadroom".into(),
                    name: "Max Headroom".into(),
                    character: "Max Headroom".into(),
                    description: "Glitchy stuttering".into(),
                },
                VoiceEntry {
                    id: "megatron".into(),
                    name: "Megatron".into(),
                    character: "Transformers".into(),
                    description: "Tyrannical warlord".into(),
                },
                VoiceEntry {
                    id: "cobra".into(),
                    name: "Cobra Commander".into(),
                    character: "G.I. Joe".into(),
                    description: "Raspy shrieking villain".into(),
                },
                VoiceEntry {
                    id: "starscream".into(),
                    name: "Starscream".into(),
                    character: "Transformers".into(),
                    description: "Screechy conniving traitor".into(),
                },
                VoiceEntry {
                    id: "soundwave".into(),
                    name: "Soundwave".into(),
                    character: "Transformers".into(),
                    description: "Monotone vocoder loyal".into(),
                },
                VoiceEntry {
                    id: "alf".into(),
                    name: "ALF".into(),
                    character: "ALF".into(),
                    description: "Sarcastic nasally alien".into(),
                },
                VoiceEntry {
                    id: "shredder".into(),
                    name: "Shredder".into(),
                    character: "TMNT".into(),
                    description: "Menacing armored villain".into(),
                },
                VoiceEntry {
                    id: "liono".into(),
                    name: "Lion-O".into(),
                    character: "ThunderCats".into(),
                    description: "Bold heroic leader".into(),
                },
                VoiceEntry {
                    id: "mummra".into(),
                    name: "Mumm-Ra".into(),
                    character: "ThunderCats".into(),
                    description: "Ancient evil sorcerer".into(),
                },
                VoiceEntry {
                    id: "krang".into(),
                    name: "Krang".into(),
                    character: "TMNT".into(),
                    description: "Whiny brain alien warlord".into(),
                },
                VoiceEntry {
                    id: "destro".into(),
                    name: "Destro".into(),
                    character: "G.I. Joe".into(),
                    description: "Smooth Scottish arms dealer".into(),
                },
                VoiceEntry {
                    id: "freddy".into(),
                    name: "Freddy Krueger".into(),
                    character: "Nightmare on Elm Street".into(),
                    description: "Sinister dream stalker".into(),
                },
            ],
        }
    }

    /// List all available voices.
    pub fn list(&self) -> &[VoiceEntry] {
        &self.voices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_synthesize() {
        let tts = TtsClient::mock();
        assert!(tts.is_mock());
        let audio = tts.synthesize("Hello world").await.unwrap();
        assert!(!audio.is_empty());
        // Should be a WAV-like header
        assert_eq!(&audio[..4], b"RIFF");
    }

    #[test]
    fn test_constructor() {
        let tts = TtsClient::new("http://localhost:8080", "orpheus", "tara");
        assert!(!tts.is_mock());
        assert_eq!(tts.model(), "orpheus");
        assert_eq!(tts.voice(), "tara");
    }

    #[tokio::test]
    async fn test_mock_different_texts() {
        let tts = TtsClient::mock();
        let a = tts.synthesize("Short").await.unwrap();
        let b = tts
            .synthesize("A much longer text with many words")
            .await
            .unwrap();
        // Mock returns same size regardless of input
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn test_voice_roster_default() {
        let roster = VoiceRoster::default_roster();
        let voices = roster.list();
        assert_eq!(voices.len(), 21);
        assert_eq!(voices[0].id, "kitt");
        assert_eq!(voices[1].id, "drclaw");
        assert_eq!(voices[9].id, "maxheadroom");
        assert_eq!(voices[10].id, "megatron");
        assert_eq!(voices[20].id, "freddy");
    }

    #[test]
    fn test_voice_roster_entries_have_all_fields() {
        let roster = VoiceRoster::default_roster();
        for v in roster.list() {
            assert!(!v.id.is_empty());
            assert!(!v.name.is_empty());
            assert!(!v.character.is_empty());
            assert!(!v.description.is_empty());
        }
    }
}
