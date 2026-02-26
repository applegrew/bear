use bear_core::{ClientMessage, ProcessInfo, ServerMessage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Notify, RwLock};
use uuid::Uuid;

// Re-export types from bear-core that bear-server code uses directly
pub use bear_core::prompts::{subagent_system_prompt, system_prompt};
pub use bear_core::{AppConfig, LlmProvider, PendingToolCall, Session};

pub const DEFAULT_BIND: &str = "127.0.0.1:49321";

// ---------------------------------------------------------------------------
// Session bus: offset-based pub-sub (Kafka-like topic per session)
// ---------------------------------------------------------------------------

/// Append-only message log shared by all consumers of a session.
/// The producer appends messages and notifies waiting consumers.
#[derive(Clone)]
pub struct TopicLog {
    messages: Arc<tokio::sync::Mutex<Vec<ServerMessage>>>,
    notify: Arc<Notify>,
}

impl TopicLog {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Append a message and wake all waiting consumers.
    pub async fn push(&self, msg: ServerMessage) {
        self.messages.lock().await.push(msg);
        self.notify.notify_waiters();
    }

    /// Create a consumer starting at offset 0 (will replay all history).
    pub fn consumer(&self) -> TopicConsumer {
        TopicConsumer {
            log: self.clone(),
            offset: 0,
        }
    }

    /// Read messages from `start` to current end. Returns the messages and
    /// the new offset (end of log at time of read).
    async fn read_from(&self, start: usize) -> (Vec<ServerMessage>, usize) {
        let log = self.messages.lock().await;
        let end = log.len();
        let msgs = log[start..end].to_vec();
        (msgs, end)
    }

    /// Current length of the log.
    async fn len(&self) -> usize {
        self.messages.lock().await.len()
    }
}

/// Per-client consumer that tracks its own offset into the topic log.
/// Guarantees ordered, exactly-once delivery — no messages are ever skipped.
pub struct TopicConsumer {
    log: TopicLog,
    offset: usize,
}

impl TopicConsumer {
    /// Wait for the next batch of messages. Returns one or more messages
    /// that the consumer hasn't seen yet. Blocks until at least one is
    /// available.  Advances the offset past the returned messages.
    #[allow(dead_code)] // used by tests
    pub async fn next_batch(&mut self) -> Vec<ServerMessage> {
        loop {
            let notified = self.log.notify.notified();
            tokio::pin!(notified);

            let (msgs, new_offset) = self.log.read_from(self.offset).await;
            if !msgs.is_empty() {
                self.offset = new_offset;
                return msgs;
            }
            notified.await;
        }
    }

    /// Peek at unconsumed messages without advancing the offset.
    /// Returns an empty vec if no new messages are available.
    pub async fn peek(&self) -> Vec<ServerMessage> {
        let (msgs, _) = self.log.read_from(self.offset).await;
        msgs
    }

    /// Wait until at least one unconsumed message is available, then peek
    /// without consuming.  Like `next_batch` but non-destructive.
    pub async fn wait_peek(&self) -> Vec<ServerMessage> {
        loop {
            let notified = self.log.notify.notified();
            tokio::pin!(notified);

            let (msgs, _) = self.log.read_from(self.offset).await;
            if !msgs.is_empty() {
                return msgs;
            }
            notified.await;
        }
    }

    /// Advance the consumer offset by `n` messages.  The caller is
    /// responsible for ensuring `n` does not exceed the number of
    /// unconsumed messages (typically after a `peek` / `wait_peek`).
    pub fn advance(&mut self, n: usize) {
        self.offset += n;
    }

    /// Current consumer offset (for computing scanned-length).
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Block until the topic log has more than `known_len` messages.
    /// Used to avoid busy-looping after a `peek` found no actionable
    /// messages — the caller passes the log length from the last peek
    /// and waits for the producer to append something new.
    pub async fn wait_changed(&self, known_len: usize) {
        loop {
            let notified = self.log.notify.notified();
            tokio::pin!(notified);

            if self.log.len().await > known_len {
                return;
            }
            notified.await;
        }
    }
}

/// Holds the pub-sub infrastructure for a session so that LLM processing
/// can continue independently of any connected client.
pub struct SessionBus {
    /// The topic log — append-only, shared by producer and all consumers.
    pub topic: TopicLog,
    /// Channel for clients to send messages to the session worker.
    pub client_tx: mpsc::Sender<ClientMessage>,
}

impl SessionBus {
    pub fn new(client_tx: mpsc::Sender<ClientMessage>) -> Self {
        Self {
            topic: TopicLog::new(),
            client_tx,
        }
    }

    /// Create a lightweight sender handle that the worker task can own.
    pub fn sender(&self) -> BusSender {
        BusSender {
            topic: self.topic.clone(),
        }
    }

    /// Create a new consumer for a connecting client. Starts at offset 0
    /// so the client receives the full message history.
    pub fn consumer(&self) -> TopicConsumer {
        self.topic.consumer()
    }
}

/// Lightweight handle for sending messages from the session worker.
#[derive(Clone)]
pub struct BusSender {
    topic: TopicLog,
}

impl BusSender {
    pub async fn send(&self, msg: ServerMessage) {
        self.topic.push(msg).await;
    }
}

#[derive(Clone)]
pub struct ServerState {
    pub sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
    pub buses: Arc<RwLock<HashMap<Uuid, SessionBus>>>,
    pub processes: Arc<RwLock<HashMap<u32, ManagedProcess>>>,
    pub config: AppConfig,
    pub http_client: reqwest::Client,
    pub rtc_peers: crate::rtc::RtcPeers,
    pub lsp_manager: Arc<crate::lsp::LspManager>,
    pub workspace_store: Arc<bear_core::workspace::WorkspaceStore>,
    pub relay_controller: Arc<crate::relay::RelayController>,
}

#[derive(Debug, Clone)]
pub struct ManagedProcess {
    pub info: ProcessInfo,
    pub session_id: Uuid,
    pub stdin_tx: Option<mpsc::Sender<String>>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn notice(text: &str) -> ServerMessage {
        ServerMessage::Notice {
            text: text.to_string(),
        }
    }

    // -- TopicLog basic operations ------------------------------------------

    #[tokio::test]
    async fn topic_log_push_and_read() {
        let log = TopicLog::new();
        log.push(notice("a")).await;
        log.push(notice("b")).await;
        let (msgs, offset) = log.read_from(0).await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(offset, 2);
    }

    #[tokio::test]
    async fn topic_log_read_from_offset() {
        let log = TopicLog::new();
        log.push(notice("a")).await;
        log.push(notice("b")).await;
        log.push(notice("c")).await;
        let (msgs, offset) = log.read_from(1).await;
        assert_eq!(msgs.len(), 2); // b, c
        assert_eq!(offset, 3);
    }

    #[tokio::test]
    async fn topic_log_read_empty() {
        let log = TopicLog::new();
        let (msgs, offset) = log.read_from(0).await;
        assert!(msgs.is_empty());
        assert_eq!(offset, 0);
    }

    #[tokio::test]
    async fn topic_log_read_at_end() {
        let log = TopicLog::new();
        log.push(notice("a")).await;
        let (msgs, offset) = log.read_from(1).await;
        assert!(msgs.is_empty());
        assert_eq!(offset, 1);
    }

    // -- TopicConsumer: replay from offset 0 --------------------------------

    #[tokio::test]
    async fn consumer_replays_full_history() {
        let log = TopicLog::new();
        log.push(notice("first")).await;
        log.push(notice("second")).await;

        let mut consumer = log.consumer();
        let batch = consumer.next_batch().await;
        assert_eq!(batch.len(), 2);
    }

    // -- TopicConsumer: next_batch blocks then wakes -------------------------

    #[tokio::test]
    async fn consumer_blocks_then_receives() {
        let log = TopicLog::new();
        let mut consumer = log.consumer();

        let log2 = log.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            log2.push(notice("delayed")).await;
        });

        let batch = consumer.next_batch().await;
        assert_eq!(batch.len(), 1);
    }

    // -- TopicConsumer: multiple batches ------------------------------------

    #[tokio::test]
    async fn consumer_multiple_batches() {
        let log = TopicLog::new();
        log.push(notice("a")).await;

        let mut consumer = log.consumer();
        let batch1 = consumer.next_batch().await;
        assert_eq!(batch1.len(), 1);

        log.push(notice("b")).await;
        log.push(notice("c")).await;
        let batch2 = consumer.next_batch().await;
        assert_eq!(batch2.len(), 2);
    }

    // -- Multiple consumers are independent ---------------------------------

    #[tokio::test]
    async fn multiple_consumers_independent() {
        let log = TopicLog::new();
        log.push(notice("a")).await;
        log.push(notice("b")).await;

        let mut c1 = log.consumer();
        let mut c2 = log.consumer();

        let b1 = c1.next_batch().await;
        assert_eq!(b1.len(), 2);

        // c1 is caught up, c2 still at offset 0
        log.push(notice("c")).await;

        let b2 = c2.next_batch().await;
        assert_eq!(b2.len(), 3); // a, b, c — full replay

        let b1_next = c1.next_batch().await;
        assert_eq!(b1_next.len(), 1); // only c
    }

    // -- Lost-wakeup prevention: push between read and await ----------------

    #[tokio::test]
    async fn no_lost_wakeup() {
        // This test verifies the notified-before-read pattern works.
        // We push a message, consume it, then push another immediately
        // and verify the consumer picks it up without hanging.
        let log = TopicLog::new();
        let mut consumer = log.consumer();

        log.push(notice("a")).await;
        let _ = consumer.next_batch().await;

        // Push and immediately try to consume — the notification must not be lost
        log.push(notice("b")).await;
        let result = tokio::time::timeout(Duration::from_millis(200), consumer.next_batch()).await;
        assert!(result.is_ok(), "consumer should not hang (lost wakeup)");
        assert_eq!(result.unwrap().len(), 1);
    }

    // -- BusSender integration ----------------------------------------------

    #[tokio::test]
    async fn bus_sender_delivers_to_consumer() {
        let (tx, _rx) = mpsc::channel::<ClientMessage>(1);
        let bus = SessionBus::new(tx);
        let sender = bus.sender();
        let mut consumer = bus.consumer();

        sender.send(notice("via sender")).await;
        let batch = consumer.next_batch().await;
        assert_eq!(batch.len(), 1);
    }

    // -- Concurrent producers and consumers ---------------------------------

    #[tokio::test]
    async fn concurrent_push_and_consume() {
        let log = TopicLog::new();
        let mut consumer = log.consumer();

        let log2 = log.clone();
        let producer = tokio::spawn(async move {
            for i in 0..20 {
                log2.push(notice(&format!("msg-{i}"))).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let mut total = 0;
        while total < 20 {
            let batch = consumer.next_batch().await;
            total += batch.len();
        }
        assert_eq!(total, 20);
        producer.await.unwrap();
    }
}
