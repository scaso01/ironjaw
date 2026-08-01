/// Rolling buffer scrubber that masks known secrets in streaming output.
///
/// Supports both single-pass (`scrub`) and streaming (`scrub_streaming`) modes.
/// In streaming mode, a rolling buffer handles secrets that span chunk boundaries.
#[derive(Debug, Clone)]
pub struct OutputScrubber {
    /// Registered secret values to mask.
    secrets: Vec<String>,
    /// Maximum length among all registered secrets.
    max_secret_len: usize,
    /// Rolling buffer for cross-chunk detection in streaming mode.
    buffer: String,
}

impl OutputScrubber {
    /// Create a new empty scrubber.
    pub fn new() -> Self {
        Self {
            secrets: Vec::new(),
            max_secret_len: 0,
            buffer: String::new(),
        }
    }

    /// Register a secret value to be masked in output.
    ///
    /// Empty strings are ignored.
    pub fn add_secret(&mut self, secret: &str) {
        if secret.is_empty() {
            return;
        }
        if secret.len() > self.max_secret_len {
            self.max_secret_len = secret.len();
        }
        self.secrets.push(secret.to_string());
    }

    /// Replace all occurrences of registered secrets with `[REDACTED]` in a single pass.
    pub fn scrub(&self, text: &str) -> String {
        let mut result = text.to_string();
        for secret in &self.secrets {
            result = result.replace(secret.as_str(), "[REDACTED]");
        }
        result
    }

    /// Process a streaming chunk, handling secrets that may span chunk boundaries.
    ///
    /// Appends the chunk to an internal rolling buffer, scrubs complete portions,
    /// and retains a tail of `max_secret_len - 1` bytes to catch cross-boundary secrets.
    /// Returns the scrubbed text that is safe to emit.
    pub fn scrub_streaming(&mut self, chunk: &str) -> String {
        if self.secrets.is_empty() {
            return chunk.to_string();
        }

        self.buffer.push_str(chunk);

        // We need to hold back max_secret_len - 1 chars in case a secret spans the boundary
        let hold_back = self.max_secret_len.saturating_sub(1);

        if self.buffer.len() <= hold_back {
            // Not enough data to emit anything safely
            return String::new();
        }

        // Scrub the full buffer
        let scrubbed = self.scrub(&self.buffer);

        // Determine how many chars to emit vs hold back
        if scrubbed.len() <= hold_back {
            self.buffer = scrubbed;
            return String::new();
        }

        let emit_len = scrubbed.len() - hold_back;
        let emitted = scrubbed[..emit_len].to_string();
        self.buffer = scrubbed[emit_len..].to_string();

        emitted
    }

    /// Flush remaining buffered content. Call when the stream is complete.
    pub fn flush(&mut self) -> String {
        let result = self.scrub(&self.buffer);
        self.buffer.clear();
        result
    }
}

impl Default for OutputScrubber {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrub_single_secret() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("sk-abc123");
        assert_eq!(
            scrubber.scrub("Your key is sk-abc123 here"),
            "Your key is [REDACTED] here"
        );
    }

    #[test]
    fn test_scrub_multiple_secrets() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("secret1");
        scrubber.add_secret("secret2");
        assert_eq!(
            scrubber.scrub("found secret1 and secret2"),
            "found [REDACTED] and [REDACTED]"
        );
    }

    #[test]
    fn test_scrub_no_secrets_registered() {
        let scrubber = OutputScrubber::new();
        assert_eq!(scrubber.scrub("nothing here"), "nothing here");
    }

    #[test]
    fn test_scrub_no_match() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("xyz");
        assert_eq!(scrubber.scrub("nothing here"), "nothing here");
    }

    #[test]
    fn test_scrub_multiple_occurrences() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("pw123");
        assert_eq!(
            scrubber.scrub("pw123 and pw123 again"),
            "[REDACTED] and [REDACTED] again"
        );
    }

    #[test]
    fn test_add_empty_secret_ignored() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("");
        assert_eq!(scrubber.secrets.len(), 0);
    }

    #[test]
    fn test_streaming_simple() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("TOKEN");
        let mut output = String::new();
        output.push_str(&scrubber.scrub_streaming("Your "));
        output.push_str(&scrubber.scrub_streaming("TOKEN is here"));
        output.push_str(&scrubber.flush());
        assert_eq!(output, "Your [REDACTED] is here");
    }

    #[test]
    fn test_streaming_cross_boundary() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("SECRET");
        let mut output = String::new();
        // "SECRET" split across two chunks: "SEC" + "RET rest"
        output.push_str(&scrubber.scrub_streaming("prefix SEC"));
        output.push_str(&scrubber.scrub_streaming("RET rest"));
        output.push_str(&scrubber.flush());
        assert_eq!(output, "prefix [REDACTED] rest");
    }

    #[test]
    fn test_streaming_no_secrets() {
        let mut scrubber = OutputScrubber::new();
        let result = scrubber.scrub_streaming("hello world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_streaming_flush_empty() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("xyz");
        let flushed = scrubber.flush();
        assert_eq!(flushed, "");
    }

    #[test]
    fn test_streaming_secret_at_end() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("END");
        let mut output = String::new();
        output.push_str(&scrubber.scrub_streaming("the END"));
        output.push_str(&scrubber.flush());
        assert_eq!(output, "the [REDACTED]");
    }

    #[test]
    fn test_streaming_multiple_chunks() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("abcdef");
        let mut output = String::new();
        output.push_str(&scrubber.scrub_streaming("x"));
        output.push_str(&scrubber.scrub_streaming("y"));
        output.push_str(&scrubber.scrub_streaming("z"));
        output.push_str(&scrubber.scrub_streaming(" abcdef done"));
        output.push_str(&scrubber.flush());
        assert_eq!(output, "xyz [REDACTED] done");
    }

    #[test]
    fn test_max_secret_len_tracking() {
        let mut scrubber = OutputScrubber::new();
        scrubber.add_secret("ab");
        assert_eq!(scrubber.max_secret_len, 2);
        scrubber.add_secret("abcde");
        assert_eq!(scrubber.max_secret_len, 5);
        scrubber.add_secret("abc");
        assert_eq!(scrubber.max_secret_len, 5);
    }

    #[test]
    fn test_default() {
        let scrubber = OutputScrubber::default();
        assert_eq!(scrubber.secrets.len(), 0);
        assert_eq!(scrubber.max_secret_len, 0);
    }
}
