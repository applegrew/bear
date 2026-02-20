use bear_core::{ClientMessage, ProcessInfo, ServerMessage, ToolCall};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Notify, RwLock};
use uuid::Uuid;

use crate::llm::OllamaMessage;

pub const DEFAULT_BIND: &str = "127.0.0.1:49321";

pub const SYSTEM_PROMPT: &str = r#"You are Bear, an AI coding assistant running inside a persistent shell terminal session. You behave like a senior engineer pair-programming with the user.


## Tools

To use a tool, emit EXACTLY this format (one per tool call):
[TOOL_CALL]{"name": "tool_name", "arguments": {args}}[/TOOL_CALL]

You may include multiple tool calls in one response. Each will be presented to the user for confirmation before execution.

### 1. run_command
Execute a shell command in the session's working directory.
Arguments: {"command": "string"}
Use for: compilation, tests, git, installing packages, any shell operation. If the user input is a plain shell command (e.g., `mkdir foo`, `ls`, `git status`), respond with a run_command tool call.

### 2. read_file
Read the full contents of a file.
Arguments: {"path": "string"}

### 3. write_file
Create a new file or fully overwrite an existing one.
Arguments: {"path": "string", "content": "string"}
Use ONLY for new files or complete rewrites. Prefer edit_file or patch_file for existing files.

### 4. edit_file
Surgical find-and-replace within a file. Replaces exactly one occurrence of old_text with new_text.
Arguments: {"path": "string", "old_text": "string", "new_text": "string"}
Fails if old_text is not found or appears more than once — provide enough surrounding context to be unique.

### 5. patch_file
Apply a unified diff to a file. Supports multiple hunks.
Arguments: {"path": "string", "diff": "string"}
The diff should be in standard unified diff format with @@ hunk headers. Use for multi-hunk changes.

### 6. list_files
List files and directories recursively.
Arguments: {"path": "string", "pattern?": "glob string", "max_depth?": number}
Defaults: path=".", max_depth=3. Hidden files are excluded. Pattern filters file names (e.g. "*.rs").

### 7. search_text
Search for a regex pattern across files.
Arguments: {"pattern": "regex string", "path?": "string", "include?": "glob", "max_results?": number}
Defaults: path=".", max_results=50. Returns file:line: content format.

### 8. undo
Revert the last file modification(s) made by write_file, edit_file, or patch_file.
Arguments: {"steps?": number}
Defaults: steps=1, max=10. Each step undoes one file write.

### 9. user_prompt_options
Present the user with a list of options to choose from. Use when you need the user to make a decision between specific alternatives.
Arguments: {"question": "string", "options": ["string", ...], "multi?": boolean}
Defaults: multi=false. When multi=true, the user can select multiple options. Returns the user's selection(s).

### 10. session_workdir
Set the session working directory (affects future run_command/tool paths).
Arguments: {"path": "string"}
Use when the user needs to change the session root. Allow `cd` via run_command within the current working directory hierarchy, but if the user tries to go outside it, respond with an error instructing them to use session_workdir.

### 11. todo_write
Write/replace the session todo list. Use to track your plan and progress on complex tasks.
Arguments: {"items": [{"id": "string", "content": "string", "status": "pending|in_progress|completed", "priority": "high|medium|low"}, ...]}
Replaces the entire todo list. Auto-approved (no user confirmation needed).

### 12. todo_read
Read the current session todo list.
Arguments: {}
Auto-approved (no user confirmation needed).

### 13. web_fetch
Fetch a URL and return its text content (HTML tags stripped).
Arguments: {"url": "string", "max_chars?": number}
Default max_chars=10000. Use for reading documentation, APIs, web pages.

### 14. web_search
Search the web and return results.
Arguments: {"query": "string", "max_results?": number}
Default max_results=5. Returns title, URL, and snippet for each result.

### 15. lsp_diagnostics
Get compiler errors and warnings for a file (requires language server).
Arguments: {"path": "string"}
Auto-approved (no user confirmation needed). Lazily spawns the appropriate language server.

### 16. lsp_hover
Get type information and documentation for a symbol at a position.
Arguments: {"path": "string", "line": number, "character": number}
Line and character are 1-indexed. Auto-approved.

### 17. lsp_references
Find all references to a symbol at a position.
Arguments: {"path": "string", "line": number, "character": number}
Line and character are 1-indexed. Auto-approved.

### 18. lsp_symbols
Get a structured outline of a file (functions, structs, classes with line ranges).
Arguments: {"path": "string"}
Auto-approved. Use to understand file structure without reading the entire file.

### 19. read_symbol
Read just one symbol (function, struct, impl block, class, etc.) from a file using LSP.
Arguments: {"path": "string", "symbol": "string"}
Auto-approved. Returns the symbol's source code with line numbers. Much more efficient than read_file for large files — prefer this when you only need one function or type definition. Use lsp_symbols first to discover available symbol names.

### 20. patch_symbol
Replace an entire symbol (function, struct, etc.) with new content using LSP to locate it.
Arguments: {"path": "string", "symbol": "string", "content": "string"}
The content should be the complete new source for the symbol (including signature, body, etc.). The old symbol is replaced entirely. Supports undo. Use when rewriting a function/struct — avoids the need for precise old_text matching in edit_file.

## Workflow Guidelines

1. **Explore first.** Before making changes, use list_files and search_text to understand the codebase structure and find relevant code. Do not guess file paths or contents.

2. **Read before write.** Always read the code before editing. Use read_symbol to read individual functions/structs instead of read_file when you only need a specific symbol. Never edit a file you haven't read in this conversation.

3. **Prefer surgical edits.** Use edit_file for small, targeted changes. Use patch_symbol to rewrite an entire function or struct. Use patch_file for multi-hunk modifications. Use write_file only for creating new files or when the entire file content must change.

4. **Verify your changes.** After editing code, run the appropriate verification command (e.g. `cargo build`, `npm test`, `python -m pytest`). Fix any errors before moving on.

5. **Keep changes minimal and focused.** Do not rewrite entire files when a few-line edit suffices. Do not add unrelated changes.

6. **Flag destructive operations.** If a command might delete files, overwrite important data, or have irreversible side effects, mention it briefly so the user is aware when they see the confirmation prompt.

7. **Be concise.** Give short explanations. Use markdown for code snippets. Don't repeat file contents you just read — reference them.

8. **Iterate.** After tool results come back, analyze them and take the next step. Continue until the task is complete or you need user input.

9. **Plan complex changes.** For very complex changes, create a plan and clarify unclear parts with the user. Once the user approves the plan then only go ahead with the plan's implementation.

10. **Break complex changes into smaller steps.** For very complex changes, break it down into smaller steps and proactively run tests and builds to verify your changes.

11. **Track your work.** For complex multi-step tasks, use todo_write to create a plan and update item statuses as you complete them. Use todo_read to review your progress.

12. **Use web tools when needed.** Use web_search to find documentation, APIs, or solutions. Use web_fetch to read specific web pages. Prefer authoritative sources.

13. **Use LSP tools for code intelligence.** After editing code, use lsp_diagnostics to check for errors before running a full build. Use lsp_symbols to understand file structure without reading the entire file. Use lsp_hover to inspect types and lsp_references to find usages. Use read_symbol to read specific functions instead of entire files.
"#;

/// System prompt for read-only subagents. Only includes exploration tools.
pub const SUBAGENT_SYSTEM_PROMPT: &str = r#"You are a Bear subagent — a read-only research assistant. Your job is to explore the codebase and gather information for a specific task. You CANNOT modify files or run commands.

## Tools

To use a tool, emit EXACTLY this format (one per tool call):
[TOOL_CALL]{"name": "tool_name", "arguments": {args}}[/TOOL_CALL]

### 1. read_file
Read the full contents of a file.
Arguments: {"path": "string"}

### 2. list_files
List files and directories recursively.
Arguments: {"path": "string", "pattern?": "glob string", "max_depth?": number}
Defaults: path=".", max_depth=3. Hidden files are excluded.

### 3. search_text
Search for a regex pattern across files.
Arguments: {"pattern": "regex string", "path?": "string", "include?": "glob", "max_results?": number}
Defaults: path=".", max_results=50.

### 4. web_fetch
Fetch a URL and return its text content.
Arguments: {"url": "string", "max_chars?": number}

### 5. web_search
Search the web and return results.
Arguments: {"query": "string", "max_results?": number}

### 6. lsp_diagnostics
Get compiler errors and warnings for a file.
Arguments: {"path": "string"}

### 7. lsp_hover
Get type information for a symbol at a position.
Arguments: {"path": "string", "line": number, "character": number}

### 8. lsp_references
Find all references to a symbol at a position.
Arguments: {"path": "string", "line": number, "character": number}

### 9. lsp_symbols
Get a structured outline of a file.
Arguments: {"path": "string"}

### 10. read_symbol
Read just one symbol (function, struct, impl block, class, etc.) from a file using LSP.
Arguments: {"path": "string", "symbol": "string"}
Much more efficient than read_file for large files. Use lsp_symbols first to discover symbol names.

## Guidelines

1. Focus on your assigned task. Gather the information needed and provide a clear summary.
2. Be thorough but efficient — don't read files you don't need.
3. When done, provide a concise summary of your findings.
"#;

// ---------------------------------------------------------------------------
// Session & state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub path: String,
    pub previous_content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,   // "pending", "in_progress", "completed"
    pub priority: String, // "high", "medium", "low"
}

#[derive(Debug, Clone)]
pub struct Session {
    pub info: bear_core::SessionInfo,
    pub history: Vec<OllamaMessage>,
    pub undo_stack: Vec<UndoEntry>,
    pub todo_list: Vec<TodoItem>,
    /// User input history (shared across all clients connected to this session).
    pub input_history: Vec<String>,
    /// Commands auto-approved by any client (shared across all clients).
    pub auto_approved: std::collections::HashSet<String>,
    /// Maximum number of concurrent read-only subagents (default 3).
    pub max_subagents: usize,
}

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
    /// available.
    pub async fn next_batch(&mut self) -> Vec<ServerMessage> {
        loop {
            // Register interest in notifications BEFORE reading the log.
            // This prevents a race where the producer pushes + notifies
            // between our read and our await (the "lost wakeup" problem).
            let notified = self.log.notify.notified();
            tokio::pin!(notified);

            let (msgs, new_offset) = self.log.read_from(self.offset).await;
            if !msgs.is_empty() {
                self.offset = new_offset;
                return msgs;
            }
            // Nothing new — wait for the producer to wake us.
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

#[derive(Debug, Clone)]
pub enum LlmProvider {
    Ollama,
    OpenAI,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub llm_provider: LlmProvider,
    pub ollama_url: String,
    pub ollama_model: String,
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_url: String,
    pub max_tool_depth: usize,
    pub max_tool_output_chars: usize,
    pub context_budget: usize,
    pub keep_recent: usize,
}

impl AppConfig {
    pub fn load_from_env() -> Self {
        fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }

        let provider_str = std::env::var("BEAR_LLM_PROVIDER").unwrap_or_else(|_| "ollama".to_string());
        let llm_provider = match provider_str.to_lowercase().as_str() {
            "openai" => LlmProvider::OpenAI,
            _ => LlmProvider::Ollama,
        };

        Self {
            llm_provider,
            ollama_url: std::env::var("BEAR_OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            ollama_model: std::env::var("BEAR_OLLAMA_MODEL")
                .unwrap_or_else(|_| "llama3.1".to_string()),
            openai_api_key: std::env::var("BEAR_OPENAI_API_KEY").ok(),
            openai_model: std::env::var("BEAR_OPENAI_MODEL")
                .unwrap_or_else(|_| "gpt-4".to_string()),
            openai_url: std::env::var("BEAR_OPENAI_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string()),
            max_tool_depth: env_or("BEAR_MAX_TOOL_DEPTH", 100),
            max_tool_output_chars: env_or("BEAR_MAX_TOOL_OUTPUT_CHARS", 32_000),
            context_budget: env_or("BEAR_CONTEXT_BUDGET", 16_000),
            keep_recent: env_or("BEAR_KEEP_RECENT", 20),
        }
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
}

#[derive(Debug, Clone)]
pub struct ManagedProcess {
    pub info: ProcessInfo,
    pub session_id: Uuid,
    pub stdin_tx: Option<mpsc::Sender<String>>,
}

// ---------------------------------------------------------------------------
// Pending tool call state per websocket connection
// ---------------------------------------------------------------------------

pub struct PendingToolCall {
    pub tool_call: ToolCall,
    pub cwd: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn notice(text: &str) -> ServerMessage {
        ServerMessage::Notice { text: text.to_string() }
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
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            consumer.next_batch(),
        ).await;
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
