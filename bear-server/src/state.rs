use bear_core::{ProcessInfo, ToolCall};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

use crate::llm::OllamaMessage;

pub const DEFAULT_BIND: &str = "127.0.0.1:49321";

pub const SYSTEM_PROMPT: &str = r#"You are Bear, an AI coding assistant running inside a persistent terminal session.

You have access to the following tools:

1. run_command - Execute a shell command in the session's working directory.
   Arguments: {"command": "shell command string"}
   The command runs in the background. The user will see its stdout/stderr. Use this for compilation, running scripts, git operations, file manipulation, etc.

2. read_file - Read the contents of a file.
   Arguments: {"path": "file path"}

3. write_file - Write content to a file (creates or overwrites).
   Arguments: {"path": "file path", "content": "file content"}

When you need to use a tool, respond with EXACTLY this JSON format on its own line:
[TOOL_CALL]{"name": "tool_name", "arguments": {args}}[/TOOL_CALL]

IMPORTANT RULES:
- Do NOT ask the user for permission before using tools. The system automatically intercepts every tool call and asks the user for confirmation before executing it. Just use tools directly when needed.
- You may include multiple tool calls in one response if needed.
- After a tool executes, you will receive its output and can continue the conversation.
- Be concise and helpful. Format code with markdown when explaining.
- If a command might be destructive (rm, overwriting files, etc.), mention it briefly so the user is aware when they see the confirmation prompt.
"#;

// ---------------------------------------------------------------------------
// Session & state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Session {
    pub info: bear_core::SessionInfo,
    pub history: Vec<OllamaMessage>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub ollama_url: String,
    pub ollama_model: String,
}

#[derive(Clone)]
pub struct ServerState {
    pub sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
    pub processes: Arc<RwLock<HashMap<u32, ManagedProcess>>>,
    pub config: AppConfig,
    pub http_client: reqwest::Client,
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
