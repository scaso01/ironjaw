//! Interactive Shell Streaming (Stream 6.2)
//!
//! Real-time output streaming for shell processes via tokio [`broadcast`]
//! channels. Multiple WebSocket clients can subscribe to the same process
//! output, enabling live tailing of long-running commands.
//!
//! Each registered process gets its own broadcast channel. Output events
//! carry sequence numbers for ordering and the originating stream
//! (stdout vs stderr).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Identifies which output stream produced a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl fmt::Display for OutputStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout => write!(f, "stdout"),
            Self::Stderr => write!(f, "stderr"),
        }
    }
}

/// An event emitted when a shell process produces output.
///
/// Subscribers receive clones of this struct via a broadcast channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOutputEvent {
    /// The process this event belongs to.
    pub process_id: String,
    /// Which output stream produced the data.
    pub stream: OutputStream,
    /// The output payload (typically one or more lines).
    pub data: String,
    /// When the output was captured.
    pub timestamp: DateTime<Utc>,
    /// Monotonically increasing sequence number within the stream.
    pub sequence: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during shell stream operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ShellStreamError {
    /// The requested process stream does not exist.
    #[error("stream not found: {0}")]
    NotFound(String),

    /// The stream has already been closed.
    #[error("stream already closed")]
    AlreadyClosed,

    /// A stream with this process ID already exists.
    #[error("stream already exists: {0}")]
    AlreadyExists(String),
}

// ---------------------------------------------------------------------------
// Internal stream state
// ---------------------------------------------------------------------------

/// Internal bookkeeping for a single process stream.
struct ShellStream {
    process_id: String,
    tx: broadcast::Sender<ShellOutputEvent>,
    started_at: DateTime<Utc>,
    line_count: u64,
    active: bool,
}

// ---------------------------------------------------------------------------
// ShellStreamManager
// ---------------------------------------------------------------------------

/// Manages broadcast channels for live shell process output.
///
/// Call [`register`](Self::register) when a new shell process is spawned,
/// then [`push_output`](Self::push_output) as output arrives. Clients
/// subscribe via [`subscribe`](Self::subscribe) and receive a
/// [`broadcast::Receiver`] that yields [`ShellOutputEvent`] values.
#[derive(Clone)]
pub struct ShellStreamManager {
    streams: Arc<RwLock<HashMap<String, ShellStream>>>,
    buffer_size: usize,
}

impl ShellStreamManager {
    /// Create a new manager. `buffer_size` controls the broadcast channel
    /// capacity (how many events can be buffered before lagging receivers
    /// start missing messages).
    pub fn new(buffer_size: usize) -> Self {
        Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
            buffer_size,
        }
    }

    /// Register a new shell process for streaming.
    ///
    /// Returns a [`broadcast::Receiver`] that the caller can use to consume
    /// output events. Additional subscribers can be added later via
    /// [`subscribe`](Self::subscribe).
    pub async fn register(
        &self,
        process_id: &str,
    ) -> Result<broadcast::Receiver<ShellOutputEvent>, ShellStreamError> {
        let mut streams = self.streams.write().await;
        if streams.contains_key(process_id) {
            return Err(ShellStreamError::AlreadyExists(process_id.to_string()));
        }

        let (tx, rx) = broadcast::channel(self.buffer_size);
        let stream = ShellStream {
            process_id: process_id.to_string(),
            tx,
            started_at: Utc::now(),
            line_count: 0,
            active: true,
        };
        streams.insert(process_id.to_string(), stream);
        Ok(rx)
    }

    /// Subscribe to an existing stream. Returns a new receiver that will
    /// receive all future events (events sent before this call are not
    /// replayed).
    pub async fn subscribe(
        &self,
        process_id: &str,
    ) -> Result<broadcast::Receiver<ShellOutputEvent>, ShellStreamError> {
        let streams = self.streams.read().await;
        let stream = streams
            .get(process_id)
            .ok_or_else(|| ShellStreamError::NotFound(process_id.to_string()))?;
        if !stream.active {
            return Err(ShellStreamError::AlreadyClosed);
        }
        Ok(stream.tx.subscribe())
    }

    /// Push output data to all subscribers of the given process.
    ///
    /// Increments the stream's line count and assigns a sequence number.
    /// It is not an error if there are no active receivers -- the event is
    /// simply dropped.
    pub async fn push_output(
        &self,
        process_id: &str,
        stream_type: OutputStream,
        data: &str,
    ) -> Result<(), ShellStreamError> {
        let mut streams = self.streams.write().await;
        let stream = streams
            .get_mut(process_id)
            .ok_or_else(|| ShellStreamError::NotFound(process_id.to_string()))?;

        if !stream.active {
            return Err(ShellStreamError::AlreadyClosed);
        }

        stream.line_count += 1;
        let event = ShellOutputEvent {
            process_id: process_id.to_string(),
            stream: stream_type,
            data: data.to_string(),
            timestamp: Utc::now(),
            sequence: stream.line_count,
        };

        // Ignore send errors (no active receivers is fine).
        let _ = stream.tx.send(event);
        Ok(())
    }

    /// Mark a stream as complete. No further output can be pushed.
    pub async fn close(&self, process_id: &str) -> Result<(), ShellStreamError> {
        let mut streams = self.streams.write().await;
        let stream = streams
            .get_mut(process_id)
            .ok_or_else(|| ShellStreamError::NotFound(process_id.to_string()))?;

        if !stream.active {
            return Err(ShellStreamError::AlreadyClosed);
        }

        stream.active = false;
        Ok(())
    }

    /// Check whether a stream is still active (not closed).
    pub async fn is_active(&self, process_id: &str) -> bool {
        self.streams
            .read()
            .await
            .get(process_id)
            .is_some_and(|s| s.active)
    }

    /// List the IDs of all currently active streams.
    pub async fn active_streams(&self) -> Vec<String> {
        self.streams
            .read()
            .await
            .values()
            .filter(|s| s.active)
            .map(|s| s.process_id.clone())
            .collect()
    }

    /// Get the total number of output events pushed to a stream.
    pub async fn line_count(&self, process_id: &str) -> Option<u64> {
        self.streams
            .read()
            .await
            .get(process_id)
            .map(|s| s.line_count)
    }

    /// Get the timestamp when a stream was registered.
    pub async fn started_at(&self, process_id: &str) -> Option<DateTime<Utc>> {
        self.streams
            .read()
            .await
            .get(process_id)
            .map(|s| s.started_at)
    }

    /// Remove all inactive (closed) streams. Returns the number removed.
    pub async fn cleanup_inactive(&self) -> usize {
        let mut streams = self.streams.write().await;
        let ids: Vec<String> = streams
            .values()
            .filter(|s| !s.active)
            .map(|s| s.process_id.clone())
            .collect();
        let count = ids.len();
        for id in ids {
            streams.remove(&id);
        }
        count
    }

    /// Total number of registered streams (both active and inactive).
    pub async fn stream_count(&self) -> usize {
        self.streams.read().await.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_creates_stream() {
        let mgr = ShellStreamManager::new(16);
        let _rx = mgr.register("proc-1").await.unwrap();
        assert!(mgr.is_active("proc-1").await);
        assert_eq!(mgr.stream_count().await, 1);
    }

    #[tokio::test]
    async fn test_register_duplicate_fails() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("proc-1").await.unwrap();
        let err = mgr.register("proc-1").await.unwrap_err();
        assert_eq!(err, ShellStreamError::AlreadyExists("proc-1".to_string()));
    }

    #[tokio::test]
    async fn test_subscribe_receives_events() {
        let mgr = ShellStreamManager::new(16);
        let _rx = mgr.register("proc-1").await.unwrap();
        let mut sub = mgr.subscribe("proc-1").await.unwrap();

        mgr.push_output("proc-1", OutputStream::Stdout, "hello world")
            .await
            .unwrap();

        let event = sub.recv().await.unwrap();
        assert_eq!(event.process_id, "proc-1");
        assert_eq!(event.stream, OutputStream::Stdout);
        assert_eq!(event.data, "hello world");
        assert_eq!(event.sequence, 1);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let mgr = ShellStreamManager::new(16);
        let mut rx1 = mgr.register("proc-1").await.unwrap();
        let mut rx2 = mgr.subscribe("proc-1").await.unwrap();

        mgr.push_output("proc-1", OutputStream::Stdout, "data")
            .await
            .unwrap();

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.data, "data");
        assert_eq!(e2.data, "data");
    }

    #[tokio::test]
    async fn test_push_output_increments_sequence() {
        let mgr = ShellStreamManager::new(16);
        let mut rx = mgr.register("proc-1").await.unwrap();

        mgr.push_output("proc-1", OutputStream::Stdout, "line 1")
            .await
            .unwrap();
        mgr.push_output("proc-1", OutputStream::Stderr, "err 1")
            .await
            .unwrap();
        mgr.push_output("proc-1", OutputStream::Stdout, "line 2")
            .await
            .unwrap();

        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();
        let e3 = rx.recv().await.unwrap();
        assert_eq!(e1.sequence, 1);
        assert_eq!(e2.sequence, 2);
        assert_eq!(e3.sequence, 3);
    }

    #[tokio::test]
    async fn test_close_marks_inactive() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("proc-1").await.unwrap();
        assert!(mgr.is_active("proc-1").await);

        mgr.close("proc-1").await.unwrap();
        assert!(!mgr.is_active("proc-1").await);
    }

    #[tokio::test]
    async fn test_close_prevents_further_output() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("proc-1").await.unwrap();
        mgr.close("proc-1").await.unwrap();

        let err = mgr
            .push_output("proc-1", OutputStream::Stdout, "nope")
            .await
            .unwrap_err();
        assert_eq!(err, ShellStreamError::AlreadyClosed);
    }

    #[tokio::test]
    async fn test_double_close_fails() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("proc-1").await.unwrap();
        mgr.close("proc-1").await.unwrap();

        let err = mgr.close("proc-1").await.unwrap_err();
        assert_eq!(err, ShellStreamError::AlreadyClosed);
    }

    #[tokio::test]
    async fn test_subscribe_closed_stream_fails() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("proc-1").await.unwrap();
        mgr.close("proc-1").await.unwrap();

        let err = mgr.subscribe("proc-1").await.unwrap_err();
        assert_eq!(err, ShellStreamError::AlreadyClosed);
    }

    #[tokio::test]
    async fn test_subscribe_nonexistent_fails() {
        let mgr = ShellStreamManager::new(16);
        let err = mgr.subscribe("nope").await.unwrap_err();
        assert_eq!(err, ShellStreamError::NotFound("nope".to_string()));
    }

    #[tokio::test]
    async fn test_push_to_nonexistent_fails() {
        let mgr = ShellStreamManager::new(16);
        let err = mgr
            .push_output("nope", OutputStream::Stdout, "data")
            .await
            .unwrap_err();
        assert_eq!(err, ShellStreamError::NotFound("nope".to_string()));
    }

    #[tokio::test]
    async fn test_close_nonexistent_fails() {
        let mgr = ShellStreamManager::new(16);
        let err = mgr.close("nope").await.unwrap_err();
        assert_eq!(err, ShellStreamError::NotFound("nope".to_string()));
    }

    #[tokio::test]
    async fn test_active_streams() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("a").await.unwrap();
        mgr.register("b").await.unwrap();
        mgr.register("c").await.unwrap();
        mgr.close("b").await.unwrap();

        let mut active = mgr.active_streams().await;
        active.sort();
        assert_eq!(active, vec!["a", "c"]);
    }

    #[tokio::test]
    async fn test_line_count() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("proc-1").await.unwrap();
        assert_eq!(mgr.line_count("proc-1").await, Some(0));

        mgr.push_output("proc-1", OutputStream::Stdout, "a")
            .await
            .unwrap();
        mgr.push_output("proc-1", OutputStream::Stderr, "b")
            .await
            .unwrap();
        assert_eq!(mgr.line_count("proc-1").await, Some(2));
    }

    #[tokio::test]
    async fn test_line_count_nonexistent() {
        let mgr = ShellStreamManager::new(16);
        assert_eq!(mgr.line_count("nope").await, None);
    }

    #[tokio::test]
    async fn test_cleanup_inactive() {
        let mgr = ShellStreamManager::new(16);
        mgr.register("a").await.unwrap();
        mgr.register("b").await.unwrap();
        mgr.register("c").await.unwrap();

        mgr.close("a").await.unwrap();
        mgr.close("c").await.unwrap();

        let removed = mgr.cleanup_inactive().await;
        assert_eq!(removed, 2);
        assert_eq!(mgr.stream_count().await, 1);
        assert!(mgr.is_active("b").await);
    }

    #[tokio::test]
    async fn test_stream_count() {
        let mgr = ShellStreamManager::new(16);
        assert_eq!(mgr.stream_count().await, 0);

        mgr.register("a").await.unwrap();
        mgr.register("b").await.unwrap();
        assert_eq!(mgr.stream_count().await, 2);

        mgr.close("a").await.unwrap();
        // Closed but not cleaned up yet.
        assert_eq!(mgr.stream_count().await, 2);

        mgr.cleanup_inactive().await;
        assert_eq!(mgr.stream_count().await, 1);
    }

    #[tokio::test]
    async fn test_is_active_nonexistent() {
        let mgr = ShellStreamManager::new(16);
        assert!(!mgr.is_active("nope").await);
    }

    #[tokio::test]
    async fn test_output_stream_display() {
        assert_eq!(OutputStream::Stdout.to_string(), "stdout");
        assert_eq!(OutputStream::Stderr.to_string(), "stderr");
    }

    #[tokio::test]
    async fn test_error_display() {
        let e = ShellStreamError::NotFound("x".to_string());
        assert_eq!(e.to_string(), "stream not found: x");

        let e = ShellStreamError::AlreadyClosed;
        assert_eq!(e.to_string(), "stream already closed");

        let e = ShellStreamError::AlreadyExists("y".to_string());
        assert_eq!(e.to_string(), "stream already exists: y");
    }

    #[tokio::test]
    async fn test_manager_is_clone() {
        let mgr = ShellStreamManager::new(16);
        let mgr2 = mgr.clone();
        mgr.register("shared").await.unwrap();
        assert!(mgr2.is_active("shared").await);
    }

    #[tokio::test]
    async fn test_push_with_no_receivers_succeeds() {
        let mgr = ShellStreamManager::new(16);
        let rx = mgr.register("proc-1").await.unwrap();
        // Drop the only receiver.
        drop(rx);

        // Pushing output with no receivers should still succeed.
        let result = mgr
            .push_output("proc-1", OutputStream::Stdout, "orphan data")
            .await;
        assert!(result.is_ok());
        assert_eq!(mgr.line_count("proc-1").await, Some(1));
    }
}
