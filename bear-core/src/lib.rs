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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandInfo {
    pub cmd: String,
    pub desc: String,
}

// ---------------------------------------------------------------------------
// Task planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: String,
    pub description: String,
    /// Whether this sub-task requires write access (file writes, commands, etc.).
    /// Read-only tasks are candidates for subagent execution.
    pub needs_write: bool,
}

// ---------------------------------------------------------------------------
// WebSocket protocol: client → server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Input { text: String },
    ToolConfirm { tool_call_id: String, approved: bool, #[serde(default)] always: bool },
    UserPromptResponse { prompt_id: String, selected: Vec<usize> },
    ProcessInput { pid: u32, text: String },
    ProcessKill { pid: u32 },
    ProcessList,
    SessionRename { name: String },
    SessionWorkdir { path: String },
    SessionEnd,
    Interrupt,
    /// User approves or rejects a proposed task plan.
    TaskPlanResponse { plan_id: String, approved: bool },
    Ping,
}

// ---------------------------------------------------------------------------
// WebSocket protocol: server → client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    SessionInfo { session: SessionInfo },
    SlashCommands { commands: Vec<SlashCommandInfo> },
    AssistantText { text: String },
    AssistantTextDone,
    ToolRequest {
        tool_call: ToolCall,
        /// For `run_command` tool calls, the list of individual command names
        /// extracted from the shell string (e.g. `["cd", "rm"]` for `cd . && rm .`).
        #[serde(skip_serializing_if = "Option::is_none")]
        extracted_commands: Option<Vec<String>>,
    },
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
    /// Sent on connect to synchronise client-side state (input history)
    /// so that any client can resume seamlessly.
    ClientState {
        input_history: Vec<String>,
    },
    /// Server auto-approved a tool call — display-only, no client response needed.
    ToolAutoApproved {
        tool_call: ToolCall,
    },
    /// A proposed task plan for the user to approve before execution.
    TaskPlan {
        plan_id: String,
        tasks: Vec<TaskItem>,
    },
    /// Progress update for a task within an approved plan.
    TaskProgress {
        plan_id: String,
        task_id: String,
        /// One of: "pending", "in_progress", "completed", "failed"
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Status update for a read-only subagent.
    SubagentUpdate {
        subagent_id: String,
        description: String,
        /// One of: "running", "completed", "failed"
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A pending tool confirmation was resolved (approved/denied) by another
    /// client. Clients showing a picker for this tool_call_id should dismiss it.
    ToolResolved {
        tool_call_id: String,
        approved: bool,
    },
    /// A pending user prompt was resolved by another client.
    /// Clients showing a picker for this prompt_id should dismiss it.
    PromptResolved {
        prompt_id: String,
    },
    Notice { text: String },
    Error { text: String },
    Thinking,
    Pong,
}
