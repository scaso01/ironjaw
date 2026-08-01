use crate::types::{AudioFormat, TranscriptionResult};
use reqwest::multipart;
use tracing::{error, info};

/// Speech-to-text client for llama-server's OpenAI-compatible transcription endpoint.
pub struct SttClient {
    client: reqwest::Client,
    base_url: String,
    model: String,
    mock: bool,
}

impl SttClient {
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            mock: false,
        }
    }

    /// Create a mock client that returns a fixed transcription.
    pub fn mock() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: String::new(),
            model: String::new(),
            mock: true,
        }
    }

    pub fn is_mock(&self) -> bool {
        self.mock
    }

    /// Transcribe audio data to text.
    pub async fn transcribe(
        &self,
        audio: &[u8],
        format: AudioFormat,
    ) -> anyhow::Result<TranscriptionResult> {
        if self.mock {
            return Ok(TranscriptionResult {
                text: "Mock transcription".to_string(),
                language: Some("en".to_string()),
                duration_secs: 1.0,
            });
        }

        let url = format!("{}/v1/audio/transcriptions", self.base_url);

        let part = multipart::Part::bytes(audio.to_vec())
            .file_name(format!("audio.{}", format.file_extension()))
            .mime_str(format.mime_type())?;

        let form = multipart::Form::new()
            .part("file", part)
            .text("model", self.model.clone())
            .text("response_format", "json");

        info!(url = %url, model = %self.model, audio_bytes = audio.len(), "STT request");

        let resp = self.client.post(&url).multipart(form).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "STT request failed");
            return Err(anyhow::anyhow!("STT failed with status {status}: {body}"));
        }

        let body: serde_json::Value = resp.json().await?;

        let text = body
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let language = body
            .get("language")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let duration_secs = body.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;

        Ok(TranscriptionResult {
            text,
            language,
            duration_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_transcription() {
        let stt = SttClient::mock();
        assert!(stt.is_mock());
        let result = stt.transcribe(&[0u8; 100], AudioFormat::Wav).await.unwrap();
        assert_eq!(result.text, "Mock transcription");
        assert_eq!(result.language, Some("en".to_string()));
    }

    #[test]
    fn test_constructor() {
        let stt = SttClient::new("http://localhost:8080", "whisper-large-v3-turbo");
        assert!(!stt.is_mock());
    }

    #[tokio::test]
    async fn test_mock_different_formats() {
        let stt = SttClient::mock();
        for format in [
            AudioFormat::Pcm16,
            AudioFormat::Wav,
            AudioFormat::Mp3,
            AudioFormat::Opus,
        ] {
            let result = stt.transcribe(&[0u8; 10], format).await.unwrap();
            assert!(!result.text.is_empty());
        }
    }
}
