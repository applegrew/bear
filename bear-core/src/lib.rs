use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49321";

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub name: Option<String>,
    pub cwd: String,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Idle,
}

impl SessionInfo {
    pub fn touch(&mut self) {
        self.last_activity = Utc::now();
    }
}

// ---------------------------------------------------------------------------
// HTTP API types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session: SessionInfo,
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub command: String,
    pub running: bool,
}

// ---------------------------------------------------------------------------
// WebSocket protocol: client → server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Input { text: String },
    ToolConfirm { tool_call_id: String, approved: bool },
    UserPromptResponse { prompt_id: String, selected: Vec<usize> },
    ProcessInput { pid: u32, text: String },
    ProcessKill { pid: u32 },
    ProcessList,
    SessionRename { name: String },
    SessionWorkdir { path: String },
    SessionEnd,
    Interrupt,
    Ping,
}

// ---------------------------------------------------------------------------
// WebSocket protocol: server → client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    SessionInfo { session: SessionInfo },
    AssistantText { text: String },
    AssistantTextDone,
    ToolRequest { tool_call: ToolCall },
    ToolOutput { tool_call_id: String, output: String },
    ProcessStarted { info: ProcessInfo },
    ProcessOutput { pid: u32, text: String },
    ProcessExited { pid: u32, code: Option<i32> },
    ProcessListResult { processes: Vec<ProcessInfo> },
    UserPrompt {
        prompt_id: String,
        question: String,
        options: Vec<String>,
        multi: bool,
    },
    SessionRenamed { name: String },
    Notice { text: String },
    Error { text: String },
    Thinking,
    Pong,
}
