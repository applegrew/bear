use axum::extract::ws::{Message, WebSocket};
use bear_core::{ClientMessage, ProcessInfo, ServerMessage, ToolCall};
use futures::StreamExt;
use uuid::Uuid;

use crate::llm::{call_ollama, OllamaMessage};
use crate::process::{handle_process_kill, handle_process_input};
use crate::state::{PendingToolCall, ServerState};
use crate::tools::{execute_tool, parse_tool_calls, strip_tool_calls};

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

pub async fn handle_socket(state: ServerState, session_id: Uuid, mut socket: WebSocket) {
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
// Helpers
// ---------------------------------------------------------------------------

pub async fn send_msg(socket: &mut WebSocket, message: ServerMessage) -> anyhow::Result<()> {
    let payload = serde_json::to_string(&message)?;
    socket.send(Message::Text(payload)).await?;
    Ok(())
}
