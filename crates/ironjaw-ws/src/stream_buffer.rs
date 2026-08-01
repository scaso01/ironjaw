//! A3: mid-stream resume — tee + swappable sink for `chat.send`'s agentic
//! loop.
//!
//! Generation already survives a socket drop: `run_agentic_chat`'s
//! `ws_closed` "drain mode" keeps consuming the LLM stream and persists the
//! final message even when nobody is listening. The gap this module closes
//! is *delivery*: if the client reconnects with the same `session_id` while
//! a run is still in flight, [`RunBuffer::attach`] swaps in the new sink and
//! replays everything sent so far as one `catchup` frame before resuming
//! live deltas — no dropped or duplicated content, and no gate on a dead
//! socket blocking generation.
//!
//! [`RunRegistry`] tracks at most one live [`RunBuffer`] per `session_id`.
//! [`RunBufferGuard`] (mirroring `agentic::InFlightGuard`) removes the entry
//! when the owning `run_agentic_chat` invocation ends, so a finished run
//! never offers a stale catchup.

use axum::extract::ws::Message;
use dashmap::DashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::handler::send_frame;
use crate::protocol::Frame;

/// Hard cap on the cumulative catchup text per run (1 MiB). Past this point,
/// deltas are still appended live to any attached sink, but are no longer
/// accumulated — a catchup replay past the cap is best-effort, not
/// unbounded.
const RUN_BUFFER_CAP_BYTES: usize = 1024 * 1024;

/// How long a run buffer may go untouched before it is considered stale.
/// Covers crashed/abandoned runs whose guard never fired, and gates
/// `attach_if_live` for very old entries.
const RUN_TTL: Duration = Duration::from_secs(600);

struct Inner {
    /// Full accumulated visible text for this run, capped at
    /// `RUN_BUFFER_CAP_BYTES`. Replayed verbatim in the `catchup` frame.
    text: String,
    /// Monotonic per-run sequence counter. The value embedded in a frame is
    /// always the counter's value *before* the increment for that frame.
    seq: u64,
    /// Set once `attach` has been called at least once.
    reattached: bool,
    /// Currently-active sink. Swapped by `attach` on reconnect.
    tx: mpsc::UnboundedSender<Message>,
    /// Last time this buffer was touched (send or attach). Drives TTL sweep.
    updated_at: Instant,
}

/// Per-run delivery buffer for one `chat.send` agentic-loop invocation.
pub struct RunBuffer {
    pub run_id: String,
    pub session_key: String,
    pub abort_token: CancellationToken,
    inner: Mutex<Inner>,
}

impl RunBuffer {
    /// Create a new buffer for `run_id`/`session_key`, initially delivering
    /// to `tx` and cancellable via `abort_token`.
    pub fn new(
        run_id: String,
        session_key: String,
        tx: mpsc::UnboundedSender<Message>,
        abort_token: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            session_key,
            abort_token,
            inner: Mutex::new(Inner {
                text: String::new(),
                seq: 0,
                reattached: false,
                tx,
                updated_at: Instant::now(),
            }),
        })
    }

    /// Append a visible text delta to the run-wide catchup buffer (subject
    /// to the 1 MiB cap) and mirror it to the current sink as the same
    /// `state: "delta"` frame the handler has always built. Send errors
    /// (dead sink — nobody currently attached) are ignored; the text is
    /// still retained for the next `attach`.
    pub fn send_delta(&self, visible: &str) {
        if visible.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.text.len() < RUN_BUFFER_CAP_BYTES {
            inner.text.push_str(visible);
        }
        let frame = Frame::event(
            "chat",
            serde_json::json!({
                "state": "delta",
                "runId": self.run_id,
                "seq": inner.seq,
                "sessionKey": self.session_key,
                "message": { "role": "assistant", "content": visible },
            }),
        );
        inner.seq += 1;
        inner.updated_at = Instant::now();
        let _ = send_frame(&inner.tx, &frame);
    }

    /// Send a pre-built frame through the current sink without touching the
    /// catchup text buffer (non-delta events: thinking markers, aborted,
    /// final, error, forced-synthesis final). Send errors are ignored for
    /// the same reason as `send_delta`.
    pub fn send_now(&self, frame: &Frame) {
        let mut inner = self.inner.lock().unwrap();
        inner.updated_at = Instant::now();
        let _ = send_frame(&inner.tx, frame);
    }

    /// Current sequence counter — the value the *next* delta will embed.
    pub fn seq(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    /// Whether this run has ever been reattached to a new sink.
    pub fn was_reattached(&self) -> bool {
        self.inner.lock().unwrap().reattached
    }

    /// Clone of the currently-active sink. Used to hand a live `tx` to
    /// `execute_tool_batch`, which streams `tool.start`/`tool.result` events
    /// independently of the delta path.
    pub fn current_tx(&self) -> mpsc::UnboundedSender<Message> {
        self.inner.lock().unwrap().tx.clone()
    }

    /// Race-free reattach seam: swap in `new_tx`, mark the run as
    /// reattached, and — under the SAME lock — send a `catchup` frame
    /// carrying the full accumulated text and the current seq to the new
    /// sink. Any `send_delta`/`send_now` call is fully ordered before or
    /// after this call (the mutex serializes them), so the new subscriber
    /// never sees a gap or a duplicate.
    pub fn attach(&self, new_tx: mpsc::UnboundedSender<Message>) {
        let mut inner = self.inner.lock().unwrap();
        inner.tx = new_tx;
        inner.reattached = true;
        let frame = Frame::event(
            "chat",
            serde_json::json!({
                "state": "catchup",
                "runId": self.run_id,
                "seq": inner.seq,
                "sessionKey": self.session_key,
                "message": { "role": "assistant", "content": inner.text.clone() },
            }),
        );
        inner.updated_at = Instant::now();
        let _ = send_frame(&inner.tx, &frame);
    }

    /// Whether this buffer has gone untouched longer than `ttl`.
    fn is_stale(&self, ttl: Duration) -> bool {
        self.inner.lock().unwrap().updated_at.elapsed() > ttl
    }

    /// Test-only: force this buffer to appear as if it hasn't been touched
    /// in `age`, without waiting on the real clock.
    #[cfg(test)]
    fn backdate(&self, age: Duration) {
        let mut inner = self.inner.lock().unwrap();
        inner.updated_at = Instant::now() - age;
    }
}

/// Registry of live [`RunBuffer`]s keyed by `session_id`. At most one live
/// run per session key at a time; entries are removed by [`RunBufferGuard`]
/// when the owning run ends (success, error, or abort).
pub struct RunRegistry {
    runs: DashMap<String, Arc<RunBuffer>>,
}

impl Default for RunRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RunRegistry {
    pub fn new() -> Self {
        Self {
            runs: DashMap::new(),
        }
    }

    /// Register a new run for `session_id`, replacing any existing entry
    /// for the same key. Opportunistically sweeps entries older than
    /// `RUN_TTL` first so a long-lived server doesn't accumulate buffers
    /// from crashed connections whose guard never fired.
    pub fn register(&self, session_id: String, buf: Arc<RunBuffer>) {
        self.sweep_stale();
        self.runs.insert(session_id, buf);
    }

    /// If a run is live for `session_id`, reattach it to `new_tx` (pushing
    /// one catchup frame) and return `true`. Returns `false` — removing the
    /// entry if present — when it is stale (untouched longer than
    /// `RUN_TTL`) or absent (no run in flight, or it already completed and
    /// its guard removed the entry).
    pub fn attach_if_live(&self, session_id: &str, new_tx: mpsc::UnboundedSender<Message>) -> bool {
        let Some(buf) = self.runs.get(session_id).map(|e| e.value().clone()) else {
            return false;
        };
        if buf.is_stale(RUN_TTL) {
            self.runs.remove(session_id);
            return false;
        }
        buf.attach(new_tx);
        true
    }

    /// Cancel the abort token of the live run for `session_id`, if any.
    /// Lets `chat.abort` cancel a run that has outlived the connection that
    /// started it: the reconnected `ConnectionContext` holds a *fresh*
    /// token, so this is the only way to reach the original token the
    /// still-running task is actually listening on.
    pub fn abort(&self, session_id: &str) {
        if let Some(buf) = self.runs.get(session_id) {
            buf.abort_token.cancel();
        }
    }

    /// Remove the entry for `session_id` only if it still belongs to
    /// `run_id`. Prevents a delayed guard drop from evicting a *newer* run
    /// registered under the same session key in the meantime. Implemented
    /// via `retain` (per-shard locked, so the check-then-remove is atomic)
    /// since this dashmap version has no `remove_if`.
    pub fn remove_if(&self, session_id: &str, run_id: &str) {
        self.runs
            .retain(|k, buf| !(k == session_id && buf.run_id == run_id));
    }

    fn sweep_stale(&self) {
        self.runs.retain(|_, buf| !buf.is_stale(RUN_TTL));
    }

    /// Test-only: current entry count, to verify sweeping actually happened.
    #[cfg(test)]
    fn debug_len(&self) -> usize {
        self.runs.len()
    }
}

/// RAII guard that removes a run's registry entry when the owning
/// `run_agentic_chat` invocation ends — success, error, or abort — mirroring
/// `agentic::InFlightGuard`. Held for the duration of the run so the buffer
/// (and therefore `attach_if_live`) disappears promptly once persistence is
/// complete, without needing a separate "done" flag.
pub struct RunBufferGuard {
    registry: Arc<RunRegistry>,
    session_id: String,
    run_id: String,
}

impl RunBufferGuard {
    pub fn new(registry: Arc<RunRegistry>, session_id: String, run_id: String) -> Self {
        Self {
            registry,
            session_id,
            run_id,
        }
    }
}

impl Drop for RunBufferGuard {
    fn drop(&mut self) {
        self.registry.remove_if(&self.session_id, &self.run_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv_frame(rx: &mut mpsc::UnboundedReceiver<Message>) -> serde_json::Value {
        match rx.try_recv().expect("expected a frame") {
            Message::Text(t) => serde_json::from_str(&t).unwrap(),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn send_delta_accumulates_text_and_increments_seq() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let buf = RunBuffer::new(
            "run-1".into(),
            "sess-1".into(),
            tx,
            CancellationToken::new(),
        );

        buf.send_delta("hello ");
        buf.send_delta("world");

        assert_eq!(buf.seq(), 2);

        let f1 = recv_frame(&mut rx);
        let f2 = recv_frame(&mut rx);
        assert_eq!(f1["payload"]["seq"], 0);
        assert_eq!(f1["payload"]["message"]["content"], "hello ");
        assert_eq!(f2["payload"]["seq"], 1);
        assert_eq!(f2["payload"]["message"]["content"], "world");
    }

    #[test]
    fn attach_seam_is_exactly_once() {
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let buf = RunBuffer::new(
            "run-2".into(),
            "sess-2".into(),
            tx1,
            CancellationToken::new(),
        );

        buf.send_delta("d1");
        buf.send_delta("d2");

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        buf.attach(tx2);

        let catchup = recv_frame(&mut rx2);
        assert_eq!(catchup["payload"]["state"], "catchup");
        assert_eq!(catchup["payload"]["message"]["content"], "d1d2");
        assert_eq!(catchup["payload"]["seq"], 2);

        buf.send_delta("d3");
        buf.send_delta("d4");

        let f3 = recv_frame(&mut rx2);
        let f4 = recv_frame(&mut rx2);
        assert_eq!(f3["payload"]["seq"], 2);
        assert_eq!(f4["payload"]["seq"], 3);

        // Old receiver only ever saw the pre-attach deltas (seq 0, 1).
        let old1 = recv_frame(&mut rx1);
        let old2 = recv_frame(&mut rx1);
        assert_eq!(old1["payload"]["seq"], 0);
        assert_eq!(old2["payload"]["seq"], 1);
        assert!(
            rx1.try_recv().is_err(),
            "old receiver must not see anything sent after attach"
        );
    }

    #[test]
    fn send_to_dropped_receiver_does_not_panic_and_attach_recovers_text() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // simulate a dead sink
        let buf = RunBuffer::new(
            "run-3".into(),
            "sess-3".into(),
            tx,
            CancellationToken::new(),
        );

        // Sends against a dead sink must not panic.
        buf.send_delta("still going");
        buf.send_delta(" strong");

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        buf.attach(tx2);

        let catchup = recv_frame(&mut rx2);
        assert_eq!(
            catchup["payload"]["message"]["content"],
            "still going strong"
        );
    }

    #[test]
    fn guard_removes_own_entry_but_not_a_newer_run() {
        let registry = Arc::new(RunRegistry::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        let buf1 = RunBuffer::new(
            "run-old".into(),
            "sess-4".into(),
            tx,
            CancellationToken::new(),
        );
        registry.register("sess-4".into(), buf1);
        let guard1 = RunBufferGuard::new(registry.clone(), "sess-4".into(), "run-old".into());

        // A newer run registers under the same session key before the old
        // guard drops (e.g. a fresh chat.send while the previous one's
        // cleanup is still pending).
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let buf2 = RunBuffer::new(
            "run-new".into(),
            "sess-4".into(),
            tx2,
            CancellationToken::new(),
        );
        registry.register("sess-4".into(), buf2);

        drop(guard1);

        let (tx3, _rx3) = mpsc::unbounded_channel();
        assert!(
            registry.attach_if_live("sess-4", tx3),
            "newer run must survive the old guard's drop"
        );
    }

    #[test]
    fn stale_run_is_removed_and_attach_if_live_returns_false() {
        let registry = RunRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let buf = RunBuffer::new(
            "run-stale".into(),
            "sess-5".into(),
            tx,
            CancellationToken::new(),
        );
        buf.backdate(RUN_TTL + Duration::from_secs(1));
        registry.register("sess-5".into(), buf);

        let (tx2, _rx2) = mpsc::unbounded_channel();
        assert!(
            !registry.attach_if_live("sess-5", tx2),
            "stale run must not be attachable"
        );

        // Confirms the entry was actually removed, not merely skipped.
        assert_eq!(registry.debug_len(), 0);
    }

    #[test]
    fn register_sweeps_stale_entries_opportunistically() {
        let registry = RunRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let stale = RunBuffer::new(
            "run-a".into(),
            "sess-a".into(),
            tx,
            CancellationToken::new(),
        );
        stale.backdate(RUN_TTL + Duration::from_secs(1));
        registry.register("sess-a".into(), stale);

        let (tx2, _rx2) = mpsc::unbounded_channel();
        let fresh = RunBuffer::new(
            "run-b".into(),
            "sess-b".into(),
            tx2,
            CancellationToken::new(),
        );
        registry.register("sess-b".into(), fresh);

        assert_eq!(
            registry.debug_len(),
            1,
            "the stale sess-a entry should have been swept on the next register"
        );
    }

    #[test]
    fn abort_cancels_the_stored_token() {
        let registry = RunRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let buf = RunBuffer::new("run-6".into(), "sess-6".into(), tx, token.clone());
        registry.register("sess-6".into(), buf);

        assert!(!token.is_cancelled());
        registry.abort("sess-6");
        assert!(token.is_cancelled());
    }

    #[test]
    fn text_buffer_stops_growing_past_cap_but_keeps_sending() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let buf = RunBuffer::new(
            "run-7".into(),
            "sess-7".into(),
            tx,
            CancellationToken::new(),
        );

        let chunk = "a".repeat(600_000);
        buf.send_delta(&chunk); // 600,000 total — under the 1 MiB cap
        buf.send_delta(&chunk); // 1,200,000 total — still appended (was under cap before this call)
        buf.send_delta(&chunk); // now over cap — this one must be skipped

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        buf.attach(tx2);
        let catchup = recv_frame(&mut rx2);
        let content = catchup["payload"]["message"]["content"].as_str().unwrap();
        assert_eq!(
            content.len(),
            1_200_000,
            "buffer must stop growing once it reaches the cap"
        );

        // All three deltas were still delivered live regardless of the cap.
        for _ in 0..3 {
            recv_frame(&mut rx);
        }
        assert!(rx.try_recv().is_err());
    }
}
