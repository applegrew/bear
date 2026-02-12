use axum::{
    extract::{ws::Message, ws::WebSocket, ws::WebSocketUpgrade, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use bear_core::{
    ClientMessage, CreateSessionRequest, CreateSessionResponse, ProcessInfo, SessionInfo,
    SessionListResponse, SessionStatus, ServerMessage, ToolCall, DEFAULT_SERVER_URL,
};
use chrono::Utc;
use fs2::FileExt;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    fs::OpenOptions,
    net::SocketAddr,
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

const DEFAULT_BIND: &str = "127.0.0.1:49321";

const SYSTEM_PROMPT: &str = r#"You are Bear, an AI coding assistant running inside a persistent terminal session.

You have access to the following tools:

1. **run_command** - Execute a shell command in the session's working directory.
   Arguments: {"command": "<shell command string>"}
   The command runs in the background. The user will see its stdout/stderr. Use this for compilation, running scripts, git operations, file manipulation, etc.

2. **read_file** - Read the contents of a file.
   Arguments: {"path": "<file path>"}

3. **write_file** - Write content to a file (creates or overwrites).
   Arguments: {"path": "<file path>", "content": "<file content>"}

When you need to use a tool, respond with EXACTLY this JSON format on its own line:
<tool_call>{"name": "<tool_name>", "arguments": {<args>}}</tool_call>

IMPORTANT RULES:
- ALWAYS ask for user confirmation before executing tool calls. Describe what you plan to do first.
- You may include multiple tool calls in one response if needed.
- After a tool executes, you will receive its output and can continue the conversation.
- Be concise and helpful. Format code with markdown when explaining.
- If a command might be destructive (rm, overwriting files, etc.), warn the user clearly.
"#;

// ---------------------------------------------------------------------------
// Session & state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Session {
    info: SessionInfo,
    history: Vec<OllamaMessage>,
}

#[derive(Debug, Clone)]
struct AppConfig {
    ollama_url: String,
    ollama_model: String,
}

#[derive(Clone)]
struct ServerState {
    sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
    processes: Arc<RwLock<HashMap<u32, ManagedProcess>>>,
    config: AppConfig,
    http_client: reqwest::Client,
}

#[derive(Debug, Clone)]
struct ManagedProcess {
    info: ProcessInfo,
    session_id: Uuid,
    stdin_tx: Option<mpsc::Sender<String>>,
}

// ---------------------------------------------------------------------------
// Pending tool call state per websocket connection
// ---------------------------------------------------------------------------

struct PendingToolCall {
    tool_call: ToolCall,
    cwd: String,
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _lock = acquire_server_lock()?;

    let config = load_config();
    tracing::info!(
        "ollama configured: url={} model={}",
        config.ollama_url,
        config.ollama_model
    );

    let state = ServerState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        processes: Arc::new(RwLock::new(HashMap::new())),
        config,
        http_client: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/ws/:session_id", get(ws_handler))
        .with_state(state)
        .layer(CorsLayer::new().allow_origin(Any).allow_headers(Any));

    let addr: SocketAddr = DEFAULT_BIND.parse()?;
    tracing::info!("bear-server running on http://{addr}");
    tracing::info!("default client url: {DEFAULT_SERVER_URL}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn acquire_server_lock() -> anyhow::Result<std::fs::File> {
    let lock_path = dirs::home_dir()
        .unwrap_or_else(|| ".".into())
        .join(".bear")
        .join("server.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)?;

    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("bear-server already running (lock held)"))?;

    Ok(file)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn list_sessions(State(state): State<ServerState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    let items = sessions.values().map(|session| session.info.clone()).collect();
    Json(SessionListResponse { sessions: items })
}

async fn create_session(
    State(state): State<ServerState>,
    Json(payload): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let cwd = payload
        .cwd
        .unwrap_or_else(|| {
            env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| ".".to_string())
        });

    let session = Session {
        info: SessionInfo {
            id: Uuid::new_v4(),
            cwd,
            created_at: Utc::now(),
            last_activity: Utc::now(),
            status: SessionStatus::Running,
        },
        history: vec![OllamaMessage {
            role: "system".to_string(),
            content: SYSTEM_PROMPT.to_string(),
        }],
    };

    let info = session.info.clone();
    state.sessions.write().await.insert(info.id, session);

    (StatusCode::CREATED, Json(CreateSessionResponse { session: info }))
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    State(state): State<ServerState>,
    Path(session_id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let exists = {
        let sessions = state.sessions.read().await;
        sessions.contains_key(&session_id)
    };
    if !exists {
        return StatusCode::NOT_FOUND.into_response();
    }

    ws.on_upgrade(move |socket| handle_socket(state, session_id, socket))
}

async fn handle_socket(state: ServerState, session_id: Uuid, mut socket: WebSocket) {
    let session_info = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.clone())
    };

    let Some(info) = session_info else {
        let _ = send_msg(&mut socket, ServerMessage::Error {
            text: "session not found".to_string(),
        }).await;
        let _ = socket.close().await;
        return;
    };

    let _ = send_msg(&mut socket, ServerMessage::SessionInfo {
        session: info.clone(),
    }).await;
    let _ = send_msg(&mut socket, ServerMessage::Notice {
        text: format!(
            "Session persists after clients disconnect. Working directory is {}.",
            info.cwd
        ),
    }).await;

    let mut pending: Option<PendingToolCall> = None;

    while let Some(Ok(msg)) = socket.next().await {
        match msg {
            Message::Text(text) => {
                let client_msg = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(m) => m,
                    Err(err) => {
                        let _ = send_msg(&mut socket, ServerMessage::Error {
                            text: format!("invalid message: {err}"),
                        }).await;
                        continue;
                    }
                };

                match client_msg {
                    ClientMessage::Input { text } => {
                        handle_user_input(
                            &state, session_id, &mut socket, &mut pending, text,
                        ).await;
                    }
                    ClientMessage::ToolConfirm { tool_call_id, approved } => {
                        handle_tool_confirm(
                            &state, session_id, &mut socket, &mut pending,
                            &tool_call_id, approved,
                        ).await;
                    }
                    ClientMessage::ProcessList => {
                        let procs = state.processes.read().await;
                        let list: Vec<ProcessInfo> = procs.values()
                            .filter(|p| p.session_id == session_id)
                            .map(|p| p.info.clone())
                            .collect();
                        let _ = send_msg(&mut socket, ServerMessage::ProcessListResult {
                            processes: list,
                        }).await;
                    }
                    ClientMessage::ProcessKill { pid } => {
                        handle_process_kill(&state, &mut socket, pid).await;
                    }
                    ClientMessage::ProcessInput { pid, text } => {
                        handle_process_input(&state, &mut socket, pid, &text).await;
                    }
                    ClientMessage::Ping => {
                        let _ = send_msg(&mut socket, ServerMessage::Pong).await;
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) => {
                let _ = socket.send(Message::Pong(vec![])).await;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// User input handling
// ---------------------------------------------------------------------------

async fn handle_user_input(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    text: String,
) {
    let user_msg = OllamaMessage {
        role: "user".to_string(),
        content: text,
    };

    let (history, cwd) = {
        let mut sessions = state.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            let _ = send_msg(socket, ServerMessage::Error {
                text: "session not found".to_string(),
            }).await;
            return;
        };
        session.info.touch();
        session.history.push(user_msg);
        (session.history.clone(), session.info.cwd.clone())
    };

    match call_ollama(&state.http_client, &state.config, &history).await {
        Ok(reply) => {
            let tool_calls = parse_tool_calls(&reply.content);
            let display_text = strip_tool_calls(&reply.content);

            {
                let mut sessions = state.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.history.push(OllamaMessage {
                        role: "assistant".to_string(),
                        content: reply.content.clone(),
                    });
                }
            }

            if !display_text.trim().is_empty() {
                let _ = send_msg(socket, ServerMessage::AssistantText {
                    text: display_text,
                }).await;
            }

            if let Some(tc) = tool_calls.into_iter().next() {
                let tool_call = ToolCall {
                    id: format!("tc_{}", Uuid::new_v4()),
                    name: tc.name,
                    arguments: tc.arguments,
                };
                let _ = send_msg(socket, ServerMessage::ToolRequest {
                    tool_call: tool_call.clone(),
                }).await;
                *pending = Some(PendingToolCall { tool_call, cwd });
            }
        }
        Err(err) => {
            let _ = send_msg(socket, ServerMessage::Error {
                text: format!("ollama request failed: {err}"),
            }).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tool confirmation
// ---------------------------------------------------------------------------

async fn handle_tool_confirm(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    tool_call_id: &str,
    approved: bool,
) {
    let ptc = match pending.take() {
        Some(p) if p.tool_call.id == tool_call_id => p,
        other => {
            *pending = other;
            let _ = send_msg(socket, ServerMessage::Error {
                text: "no matching pending tool call".to_string(),
            }).await;
            return;
        }
    };

    if !approved {
        let output = "Tool call rejected by user.".to_string();
        let _ = send_msg(socket, ServerMessage::ToolOutput {
            tool_call_id: ptc.tool_call.id.clone(),
            output: output.clone(),
        }).await;
        append_tool_result(state, session_id, &output).await;
        return;
    }

    let output = execute_tool(state, session_id, socket, &ptc).await;
    let _ = send_msg(socket, ServerMessage::ToolOutput {
        tool_call_id: ptc.tool_call.id.clone(),
        output: output.clone(),
    }).await;
    append_tool_result(state, session_id, &output).await;
}

async fn append_tool_result(state: &ServerState, session_id: Uuid, output: &str) {
    let mut sessions = state.sessions.write().await;
    if let Some(session) = sessions.get_mut(&session_id) {
        session.history.push(OllamaMessage {
            role: "user".to_string(),
            content: format!("[Tool output]:\n{output}"),
        });
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

async fn execute_tool(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    ptc: &PendingToolCall,
) -> String {
    match ptc.tool_call.name.as_str() {
        "run_command" => {
            let cmd_str = ptc.tool_call.arguments["command"]
                .as_str()
                .unwrap_or("echo 'no command'")
                .to_string();
            execute_run_command(state, session_id, socket, &cmd_str, &ptc.cwd).await
        }
        "read_file" => {
            let path = ptc.tool_call.arguments["path"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let full_path = if path.starts_with('/') {
                path
            } else {
                format!("{}/{}", ptc.cwd, path)
            };
            match tokio::fs::read_to_string(&full_path).await {
                Ok(content) => content,
                Err(err) => format!("Error reading {full_path}: {err}"),
            }
        }
        "write_file" => {
            let path = ptc.tool_call.arguments["path"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let content = ptc.tool_call.arguments["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let full_path = if path.starts_with('/') {
                path
            } else {
                format!("{}/{}", ptc.cwd, path)
            };
            if let Some(parent) = std::path::Path::new(&full_path).parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            match tokio::fs::write(&full_path, &content).await {
                Ok(()) => format!("Written {} bytes to {full_path}", content.len()),
                Err(err) => format!("Error writing {full_path}: {err}"),
            }
        }
        other => format!("Unknown tool: {other}"),
    }
}

async fn execute_run_command(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    cmd_str: &str,
    cwd: &str,
) -> String {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd_str)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return format!("Failed to spawn: {err}"),
    };

    let pid = child.id().unwrap_or(0);
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(16);

    let proc_info = ProcessInfo {
        pid,
        command: cmd_str.to_string(),
        running: true,
    };

    state.processes.write().await.insert(pid, ManagedProcess {
        info: proc_info.clone(),
        session_id,
        stdin_tx: Some(stdin_tx),
    });

    let _ = send_msg(socket, ServerMessage::ProcessStarted {
        info: proc_info,
    }).await;

    let mut stdin_handle = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let processes = state.processes.clone();
    let (output_tx, mut output_rx) = mpsc::channel::<String>(64);

    if let Some(stdout) = stdout {
        let tx = output_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }

    if let Some(stderr) = stderr {
        let tx = output_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }
    drop(output_tx);

    tokio::spawn(async move {
        while let Some(data) = stdin_rx.recv().await {
            if let Some(ref mut stdin) = stdin_handle {
                let _ = stdin.write_all(data.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            }
        }
    });

    let mut all_output = String::new();
    while let Some(line) = output_rx.recv().await {
        all_output.push_str(&line);
        all_output.push('\n');
    }

    let status = child.wait().await;
    let code = status.ok().and_then(|s| s.code());

    {
        let mut procs = processes.write().await;
        if let Some(p) = procs.get_mut(&pid) {
            p.info.running = false;
            p.stdin_tx = None;
        }
    }

    let _ = send_msg(socket, ServerMessage::ProcessExited { pid, code }).await;

    if all_output.is_empty() {
        format!("Process exited with code {}", code.map(|c| c.to_string()).unwrap_or("unknown".into()))
    } else {
        all_output
    }
}

// ---------------------------------------------------------------------------
// Process management helpers
// ---------------------------------------------------------------------------

async fn handle_process_kill(
    state: &ServerState,
    socket: &mut WebSocket,
    pid: u32,
) {
    use std::process::Command as StdCommand;
    let exists = state.processes.read().await.contains_key(&pid);
    if !exists {
        let _ = send_msg(socket, ServerMessage::Error {
            text: format!("No managed process with pid {pid}"),
        }).await;
        return;
    }

    let _ = StdCommand::new("kill").arg(pid.to_string()).status();
    let mut procs = state.processes.write().await;
    if let Some(p) = procs.get_mut(&pid) {
        p.info.running = false;
        p.stdin_tx = None;
    }
    let _ = send_msg(socket, ServerMessage::ProcessExited { pid, code: None }).await;
}

async fn handle_process_input(
    state: &ServerState,
    socket: &mut WebSocket,
    pid: u32,
    text: &str,
) {
    let procs = state.processes.read().await;
    if let Some(p) = procs.get(&pid) {
        if let Some(tx) = &p.stdin_tx {
            let _ = tx.send(text.to_string()).await;
        } else {
            let _ = send_msg(socket, ServerMessage::Error {
                text: format!("Process {pid} stdin closed"),
            }).await;
        }
    } else {
        let _ = send_msg(socket, ServerMessage::Error {
            text: format!("No managed process with pid {pid}"),
        }).await;
    }
}

// ---------------------------------------------------------------------------
// Tool call parsing from LLM output
// ---------------------------------------------------------------------------

struct ParsedToolCall {
    name: String,
    arguments: serde_json::Value,
}

fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("<tool_call>") {
        let after_tag = &remaining[start + 11..];
        if let Some(end) = after_tag.find("</tool_call>") {
            let json_str = &after_tag[..end].trim();
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                let name = val["name"].as_str().unwrap_or("").to_string();
                let arguments = val["arguments"].clone();
                if !name.is_empty() {
                    calls.push(ParsedToolCall { name, arguments });
                }
            }
            remaining = &after_tag[end + 12..];
        } else {
            break;
        }
    }
    calls
}

fn strip_tool_calls(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<tool_call>") {
        if let Some(end) = result[start..].find("</tool_call>") {
            result.replace_range(start..start + end + 12, "");
        } else {
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Ollama types & API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
}

fn load_config() -> AppConfig {
    let ollama_url = env::var("BEAR_OLLAMA_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    let ollama_model = env::var("BEAR_OLLAMA_MODEL")
        .unwrap_or_else(|_| "llama3.1".to_string());
    AppConfig {
        ollama_url,
        ollama_model,
    }
}

async fn call_ollama(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[OllamaMessage],
) -> anyhow::Result<OllamaMessage> {
    let url = format!("{}/api/chat", config.ollama_url.trim_end_matches('/'));
    let payload = OllamaChatRequest {
        model: config.ollama_model.clone(),
        messages: messages.to_vec(),
        stream: false,
    };

    let response = http_client
        .post(&url)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("ollama returned {status}: {body}");
        anyhow::bail!("ollama returned {status}: {body}");
    }

    let body: OllamaChatResponse = response.json().await?;
    Ok(body.message)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send_msg(socket: &mut WebSocket, message: ServerMessage) -> anyhow::Result<()> {
    let payload = serde_json::to_string(&message)?;
    socket.send(Message::Text(payload)).await?;
    Ok(())
}
