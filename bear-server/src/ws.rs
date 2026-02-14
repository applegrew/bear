use std::collections::VecDeque;

use axum::extract::ws::{Message, WebSocket};
use bear_core::{ClientMessage, ProcessInfo, ServerMessage, SlashCommandInfo, ToolCall};
use futures::StreamExt;
use tokio::process::Command;
use uuid::Uuid;

use crate::llm::{call_ollama_streaming, compact_history_if_needed, reflective_thinking, OllamaMessage};
use crate::process::{cleanup_session_processes, handle_process_kill, handle_process_input};
use crate::state::{PendingToolCall, ServerState};
use crate::tools::{execute_tool, parse_tool_calls};

/// When true, run a non-streaming reflection call before the main LLM response.
const ENABLE_REFLECTION: bool = true;

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/ps", "List background processes"),
    ("/kill", "Kill a background process (usage: /kill <pid>)"),
    ("/send", "Send stdin to a process (usage: /send <pid> <text>)"),
    ("/session name", "Name the current session (usage: /session name <n>)"),
    ("/session workdir", "Set session working directory (usage: /session workdir <path>)"),
    ("/allowed", "Show auto-approved commands"),
    ("/exit", "Disconnect, keep session alive"),
    ("/end", "End session, pick another"),
    ("/help", "Show help"),
];

fn slash_command_infos() -> Vec<SlashCommandInfo> {
    SLASH_COMMANDS
        .iter()
        .map(|(cmd, desc)| SlashCommandInfo {
            cmd: (*cmd).to_string(),
            desc: (*desc).to_string(),
        })
        .collect()
}

/// Tracks a user_prompt_options tool that is waiting for the client's selection.
struct PendingPrompt {
    prompt_id: String,
    tool_call: PendingToolCall,
    options: Vec<String>,
    multi: bool,
}

/// Tracks a tool-depth continuation prompt waiting for the user's choice.
struct PendingDepthPrompt {
    prompt_id: String,
}

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
    let _ = send_msg(&mut socket, ServerMessage::SlashCommands {
        commands: slash_command_infos(),
    }).await;
    let _ = send_msg(&mut socket, ServerMessage::Notice {
        text: format!(
            "Session persists after clients disconnect. Working directory is {}.",
            info.cwd
        ),
    }).await;

    if info.name.is_none() {
        let _ = send_msg(&mut socket, ServerMessage::Notice {
            text: "Tip: Name this session with /session name <name>".to_string(),
        }).await;
    }

    let mut pending: Option<PendingToolCall> = None;
    let mut tool_queue: VecDeque<PendingToolCall> = VecDeque::new();
    let mut tool_depth: usize = 0;
    let mut pending_prompt: Option<PendingPrompt> = None;
    let mut pending_depth_prompt: Option<PendingDepthPrompt> = None;
    let mut depth_limit: usize = state.config.max_tool_depth;
    let mut bulk_increment: usize = 50;

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
                        tool_depth = 0;
                        depth_limit = state.config.max_tool_depth;
                        bulk_increment = 50;
                        handle_user_input(
                            &state, session_id, &mut socket,
                            &mut pending, &mut pending_prompt,
                            &mut pending_depth_prompt,
                            &mut tool_queue, &mut tool_depth,
                            &mut depth_limit, &mut bulk_increment,
                            text,
                        ).await;
                    }
                    ClientMessage::ToolConfirm { tool_call_id, approved } => {
                        handle_tool_confirm(
                            &state, session_id, &mut socket,
                            &mut pending, &mut pending_prompt,
                            &mut pending_depth_prompt,
                            &mut tool_queue, &mut tool_depth,
                            &mut depth_limit, &mut bulk_increment,
                            &tool_call_id, approved,
                        ).await;
                    }
                    ClientMessage::UserPromptResponse { prompt_id, selected } => {
                        // Check if this is a depth-continuation prompt response
                        if let Some(dp) = pending_depth_prompt.take() {
                            if dp.prompt_id == prompt_id {
                                handle_depth_prompt_response(
                                    &state, session_id, &mut socket,
                                    &mut pending, &mut pending_prompt,
                                    &mut pending_depth_prompt,
                                    &mut tool_queue, &mut tool_depth,
                                    &mut depth_limit, &mut bulk_increment,
                                    selected,
                                ).await;
                            } else {
                                // Not ours, restore it
                                pending_depth_prompt = Some(dp);
                            }
                        } else {
                            handle_prompt_response(
                                &state, session_id, &mut socket,
                                &mut pending, &mut pending_prompt,
                                &mut pending_depth_prompt,
                                &mut tool_queue, &mut tool_depth,
                                &mut depth_limit, &mut bulk_increment,
                                &prompt_id, selected,
                            ).await;
                        }
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
                    ClientMessage::SessionRename { name } => {
                        let trimmed = name.trim().to_string();
                        if trimmed.is_empty() {
                            let _ = send_msg(&mut socket, ServerMessage::Error {
                                text: "Session name must not be empty.".to_string(),
                            }).await;
                        } else {
                            let mut sessions = state.sessions.write().await;
                            if let Some(session) = sessions.get_mut(&session_id) {
                                session.info.name = Some(trimmed.clone());
                            }
                            let _ = send_msg(&mut socket, ServerMessage::SessionRenamed {
                                name: trimmed,
                            }).await;
                        }
                    }
                    ClientMessage::SessionWorkdir { path } => {
                        let trimmed = path.trim();
                        if trimmed.is_empty() {
                            let _ = send_msg(&mut socket, ServerMessage::Error {
                                text: "Usage: /session workdir <path>".to_string(),
                            }).await;
                            continue;
                        }

                        let current_cwd = {
                            let sessions = state.sessions.read().await;
                            sessions.get(&session_id).map(|s| s.info.cwd.clone())
                        };

                        let Some(current_cwd) = current_cwd else {
                            let _ = send_msg(&mut socket, ServerMessage::Error {
                                text: "session not found".to_string(),
                            }).await;
                            continue;
                        };

                        let cmd = format!("cd {trimmed} && pwd");
                        match Command::new("sh")
                            .arg("-lc")
                            .arg(cmd)
                            .current_dir(&current_cwd)
                            .output()
                            .await
                        {
                            Ok(out) if out.status.success() => {
                                let new_cwd = String::from_utf8_lossy(&out.stdout)
                                    .trim()
                                    .to_string();
                                if new_cwd.is_empty() {
                                    let _ = send_msg(&mut socket, ServerMessage::Error {
                                        text: "Failed to resolve working directory.".to_string(),
                                    }).await;
                                    continue;
                                }

                                let updated_session = {
                                    let mut sessions = state.sessions.write().await;
                                    if let Some(session) = sessions.get_mut(&session_id) {
                                        session.info.cwd = new_cwd.clone();
                                        session.info.touch();
                                        Some(session.info.clone())
                                    } else {
                                        None
                                    }
                                };

                                if let Some(session) = updated_session {
                                    let _ = send_msg(&mut socket, ServerMessage::Notice {
                                        text: format!("Working directory set to: {new_cwd}"),
                                    }).await;
                                    let _ = send_msg(&mut socket, ServerMessage::SessionInfo {
                                        session,
                                    }).await;
                                }
                            }
                            Ok(out) => {
                                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                                let msg = if err.is_empty() {
                                    "Failed to change directory.".to_string()
                                } else {
                                    format!("Failed to change directory: {err}")
                                };
                                let _ = send_msg(&mut socket, ServerMessage::Error {
                                    text: msg,
                                }).await;
                            }
                            Err(err) => {
                                let _ = send_msg(&mut socket, ServerMessage::Error {
                                    text: format!("Failed to change directory: {err}"),
                                }).await;
                            }
                        }
                    }
                    ClientMessage::SessionEnd => {
                        // Remove session and clean up processes
                        cleanup_session_processes(&state, session_id).await;
                        {
                            let mut sessions = state.sessions.write().await;
                            sessions.remove(&session_id);
                        }
                        let _ = send_msg(&mut socket, ServerMessage::Notice {
                            text: "Session ended and removed.".to_string(),
                        }).await;
                        break;
                    }
                    ClientMessage::Interrupt => {
                        // No-op outside of streaming — handled inside invoke_llm
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

    // Client disconnected — clean up any running processes for this session
    cleanup_session_processes(&state, session_id).await;
}

// ---------------------------------------------------------------------------
// User input handling
// ---------------------------------------------------------------------------

async fn handle_slash_command(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    text: &str,
) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }

    match trimmed {
        "/ps" => {
            let procs = state.processes.read().await;
            let list: Vec<ProcessInfo> = procs.values()
                .filter(|p| p.session_id == session_id)
                .map(|p| p.info.clone())
                .collect();
            let _ = send_msg(socket, ServerMessage::ProcessListResult {
                processes: list,
            }).await;
            return true;
        }
        "/allowed" => {
            let _ = send_msg(socket, ServerMessage::Notice {
                text: "Auto-approved commands are tracked per client. Use /allowed in the client UI.".to_string(),
            }).await;
            return true;
        }
        "/help" => {
            let mut lines = Vec::with_capacity(SLASH_COMMANDS.len() + 1);
            lines.push("Commands:".to_string());
            for (cmd, desc) in SLASH_COMMANDS {
                lines.push(format!("  {cmd:<20} {desc}"));
            }
            let help = lines.join("\n");
            let _ = send_msg(socket, ServerMessage::Notice {
                text: help,
            }).await;
            return true;
        }
        "/exit" => {
            let _ = send_msg(socket, ServerMessage::Notice {
                text: "Disconnecting. Session preserved.".to_string(),
            }).await;
            let _ = socket.send(Message::Close(None)).await;
            return true;
        }
        "/end" => {
            cleanup_session_processes(state, session_id).await;
            {
                let mut sessions = state.sessions.write().await;
                sessions.remove(&session_id);
            }
            let _ = send_msg(socket, ServerMessage::Notice {
                text: "Session ended and removed.".to_string(),
            }).await;
            let _ = socket.send(Message::Close(None)).await;
            return true;
        }
        _ => {}
    }

    if let Some(rest) = trimmed.strip_prefix("/kill ") {
        match rest.trim().parse::<u32>() {
            Ok(pid) => {
                handle_process_kill(state, socket, pid).await;
            }
            Err(_) => {
                let _ = send_msg(socket, ServerMessage::Error {
                    text: "Usage: /kill <pid>".to_string(),
                }).await;
            }
        }
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("/send ") {
        if let Some((pid_str, input)) = rest.split_once(' ') {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                handle_process_input(state, socket, pid, input).await;
            } else {
                let _ = send_msg(socket, ServerMessage::Error {
                    text: "Usage: /send <pid> <text>".to_string(),
                }).await;
            }
        } else {
            let _ = send_msg(socket, ServerMessage::Error {
                text: "Usage: /send <pid> <text>".to_string(),
            }).await;
        }
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("/session ") {
        if let Some(name) = rest.strip_prefix("name ") {
            let name = name.trim();
            if name.is_empty() {
                let _ = send_msg(socket, ServerMessage::Error {
                    text: "Usage: /session name <session name>".to_string(),
                }).await;
            } else {
                let mut sessions = state.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.info.name = Some(name.to_string());
                }
                let _ = send_msg(socket, ServerMessage::SessionRenamed {
                    name: name.to_string(),
                }).await;
            }
            return true;
        }

        if let Some(path) = rest.strip_prefix("workdir ") {
            let path = path.trim();
            if path.is_empty() {
                let _ = send_msg(socket, ServerMessage::Error {
                    text: "Usage: /session workdir <path>".to_string(),
                }).await;
                return true;
            }

            let current_cwd = {
                let sessions = state.sessions.read().await;
                sessions.get(&session_id).map(|s| s.info.cwd.clone())
            };

            let Some(current_cwd) = current_cwd else {
                let _ = send_msg(socket, ServerMessage::Error {
                    text: "session not found".to_string(),
                }).await;
                return true;
            };

            let cmd = format!("cd {path} && pwd");
            match Command::new("sh")
                .arg("-lc")
                .arg(cmd)
                .current_dir(&current_cwd)
                .output()
                .await
            {
                Ok(out) if out.status.success() => {
                    let new_cwd = String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .to_string();
                    if new_cwd.is_empty() {
                        let _ = send_msg(socket, ServerMessage::Error {
                            text: "Failed to resolve working directory.".to_string(),
                        }).await;
                        return true;
                    }

                    let updated_session = {
                        let mut sessions = state.sessions.write().await;
                        if let Some(session) = sessions.get_mut(&session_id) {
                            session.info.cwd = new_cwd.clone();
                            session.info.touch();
                            Some(session.info.clone())
                        } else {
                            None
                        }
                    };

                    if let Some(session) = updated_session {
                        let _ = send_msg(socket, ServerMessage::Notice {
                            text: format!("Working directory set to: {new_cwd}"),
                        }).await;
                        let _ = send_msg(socket, ServerMessage::SessionInfo {
                            session,
                        }).await;
                    }
                }
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let msg = if err.is_empty() {
                        "Failed to change directory.".to_string()
                    } else {
                        format!("Failed to change directory: {err}")
                    };
                    let _ = send_msg(socket, ServerMessage::Error { text: msg }).await;
                }
                Err(err) => {
                    let _ = send_msg(socket, ServerMessage::Error {
                        text: format!("Failed to change directory: {err}"),
                    }).await;
                }
            }
            return true;
        }

        let _ = send_msg(socket, ServerMessage::Error {
            text: "Usage: /session name <session name> OR /session workdir <path>".to_string(),
        }).await;
        return true;
    }

    let _ = send_msg(socket, ServerMessage::Error {
        text: format!("Unknown command: {trimmed}"),
    }).await;
    true
}

async fn handle_user_input(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    text: String,
) {
    if handle_slash_command(state, session_id, socket, &text).await {
        return;
    }

    let user_msg = OllamaMessage {
        role: "user".to_string(),
        content: text,
    };

    {
        let mut sessions = state.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            let _ = send_msg(socket, ServerMessage::Error {
                text: "session not found".to_string(),
            }).await;
            return;
        };
        session.info.touch();
        session.history.push(user_msg);
    }

    invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
}

/// Call the LLM with current history, send assistant text, and queue all
/// tool calls for sequential user confirmation.
/// When `ENABLE_REFLECTION` is true, a non-streaming reflection call runs
/// first and its output is temporarily injected into the history so the main
/// streaming response benefits from the reasoning.
async fn invoke_llm(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
) {
    if *tool_depth >= *depth_limit {
        // Ask the user whether to continue instead of hard-aborting
        let prompt_id = format!("depth_{}", Uuid::new_v4());
        let options = vec![
            "Yes".to_string(),
            format!("Yes, for next {}", *bulk_increment),
            "No".to_string(),
        ];
        let _ = send_msg(socket, ServerMessage::UserPrompt {
            prompt_id: prompt_id.clone(),
            question: format!(
                "Tool depth limit reached ({} consecutive calls). Continue?",
                *tool_depth,
            ),
            options: options.clone(),
            multi: false,
        }).await;
        *pending_depth_prompt = Some(PendingDepthPrompt { prompt_id });
        return;
    }

    let _ = send_msg(socket, ServerMessage::Thinking).await;

    // Compact history if it exceeds the token budget
    {
        let mut sessions = state.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            compact_history_if_needed(
                &state.http_client,
                &state.config,
                &mut session.history,
            ).await;
        }
    }

    let (history, cwd) = {
        let sessions = state.sessions.read().await;
        let Some(session) = sessions.get(&session_id) else {
            let _ = send_msg(socket, ServerMessage::Error {
                text: "session not found".to_string(),
            }).await;
            return;
        };
        (session.history.clone(), session.info.cwd.clone())
    };

    let session_context = format!("Session context:\n- Working directory: {cwd}");
    let mut history_for_llm = history.clone();
    if let Some(system_msg) = history_for_llm.first_mut() {
        system_msg.content = format!("{}\n\n{session_context}", system_msg.content);
    }

    // Optional reflection: run a non-streaming call to reason about the
    // problem first, then inject the reflection into the history so the
    // main streaming response benefits from it. The reflection is NOT
    // persisted to the session history — it only influences this call.
    if ENABLE_REFLECTION {
        match reflective_thinking(&state.http_client, &state.config, &history_for_llm, &session_context).await {
            Ok(reflection) => {
                tracing::debug!("reflection complete ({} chars)", reflection.content.len());
                history_for_llm.push(reflection);
            }
            Err(err) => {
                tracing::warn!("reflective thinking failed, continuing without it: {err}");
            }
        }
    }

    // Stream LLM response — send chunks to client as they arrive
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<String>(64);

    let http = state.http_client.clone();
    let cfg = state.config.clone();
    let llm_handle = tokio::spawn(async move {
        call_ollama_streaming(&http, &cfg, &history_for_llm, &chunk_tx).await
    });

    // Forward chunks to the client as AssistantText, but also listen
    // for an Interrupt message on the socket. If interrupted, abort the
    // LLM task, save partial output, and return the new user input.
    let mut partial_content = String::new();
    let mut interrupted_input: Option<String> = None;

    loop {
        tokio::select! {
            chunk = chunk_rx.recv() => {
                match chunk {
                    Some(text) => {
                        partial_content.push_str(&text);
                        let _ = send_msg(socket, ServerMessage::AssistantText {
                            text,
                        }).await;
                    }
                    None => break, // channel closed, LLM done
                }
            }
            ws_msg = socket.next() => {
                if let Some(Ok(Message::Text(raw))) = ws_msg {
                    if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&raw) {
                        match client_msg {
                            ClientMessage::Interrupt => {
                                // Abort the LLM task
                                llm_handle.abort();
                                // Drain remaining chunks
                                while chunk_rx.try_recv().is_ok() {}
                                break;
                            }
                            ClientMessage::Input { text } => {
                                // User sent new input — treat as interrupt + new request
                                llm_handle.abort();
                                while chunk_rx.try_recv().is_ok() {}
                                interrupted_input = Some(text);
                                break;
                            }
                            _ => {
                                // Ignore other messages during streaming
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = send_msg(socket, ServerMessage::AssistantTextDone).await;

    // Save partial or full response to history
    if !partial_content.is_empty() {
        let mut sessions = state.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.history.push(OllamaMessage {
                role: "assistant".to_string(),
                content: partial_content.clone(),
            });
        }
    }

    // If interrupted with new input, process it immediately
    if let Some(new_input) = interrupted_input {
        *tool_depth = 0;
        *depth_limit = state.config.max_tool_depth;
        *bulk_increment = 50;
        tool_queue.clear();

        let user_msg = OllamaMessage {
            role: "user".to_string(),
            content: new_input,
        };
        {
            let mut sessions = state.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.info.touch();
                session.history.push(user_msg);
            }
        }
        // Recurse to handle the new input
        Box::pin(invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
        return;
    }

    // Normal completion — check for tool calls
    match llm_handle.await {
        Ok(Ok(reply)) => {
            let tool_calls = parse_tool_calls(&reply.content);

            // Queue all tool calls
            tool_queue.clear();
            for tc in tool_calls {
                let tool_call = ToolCall {
                    id: format!("tc_{}", Uuid::new_v4()),
                    name: tc.name,
                    arguments: tc.arguments,
                };
                tool_queue.push_back(PendingToolCall {
                    tool_call,
                    cwd: cwd.clone(),
                });
            }

            // Present the first tool call to the user
            Box::pin(present_next_tool(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
        }
        Ok(Err(err)) => {
            let _ = send_msg(socket, ServerMessage::Error {
                text: format!("ollama request failed: {err}"),
            }).await;
        }
        Err(err) => {
            // JoinError — could be a panic or abort (from interrupt)
            if err.is_cancelled() {
                // Interrupted — already handled above
            } else {
                let _ = send_msg(socket, ServerMessage::Error {
                    text: format!("LLM task panicked: {err}"),
                }).await;
            }
        }
    }
}

/// Pop the next tool call from the queue. For most tools, send a ToolRequest
/// to the client for confirmation. For `user_prompt_options`, skip the
/// confirmation step entirely and directly send the UserPrompt.
async fn present_next_tool(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
) {
    let Some(ptc) = tool_queue.pop_front() else { return };

    // Auto-handle user_prompt_options without showing a tool confirmation
    if ptc.tool_call.name == "user_prompt_options" {
        let args = &ptc.tool_call.arguments;
        let question = args["question"].as_str().unwrap_or("Choose:").to_string();
        let options: Vec<String> = args["options"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let multi = args["multi"].as_bool().unwrap_or(false);

        if options.is_empty() {
            let output = "Error: user_prompt_options requires a non-empty 'options' array.".to_string();
            let _ = send_msg(socket, ServerMessage::ToolOutput {
                tool_call_id: ptc.tool_call.id.clone(),
                output: output.clone(),
            }).await;
            append_tool_result(state, session_id, &output).await;
            *tool_depth += 1;
            if !tool_queue.is_empty() {
                // Recurse for next tool (box pin to allow recursion)
                Box::pin(present_next_tool(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
            } else {
                invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
            }
            return;
        }

        let prompt_id = format!("prompt_{}", Uuid::new_v4());
        let _ = send_msg(socket, ServerMessage::UserPrompt {
            prompt_id: prompt_id.clone(),
            question: question.clone(),
            options: options.clone(),
            multi,
        }).await;

        *pending_prompt = Some(PendingPrompt {
            prompt_id,
            tool_call: ptc,
            options,
            multi,
        });
        return;
    }

    let _ = send_msg(socket, ServerMessage::ToolRequest {
        tool_call: ptc.tool_call.clone(),
    }).await;
    *pending = Some(ptc);
}

// ---------------------------------------------------------------------------
// Tool confirmation
// ---------------------------------------------------------------------------

async fn handle_tool_confirm(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
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
        // If rejected, skip remaining queued tools and re-invoke LLM
        tool_queue.clear();
        *tool_depth += 1;
        invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        return;
    }

    let output = execute_tool(state, session_id, socket, &ptc).await;
    let _ = send_msg(socket, ServerMessage::ToolOutput {
        tool_call_id: ptc.tool_call.id.clone(),
        output: output.clone(),
    }).await;
    append_tool_result(state, session_id, &output).await;
    *tool_depth += 1;

    // If more tool calls queued from the same LLM response, present the next one
    if !tool_queue.is_empty() {
        present_next_tool(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    } else {
        // All tools from this response executed — re-invoke LLM
        invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    }
}

// ---------------------------------------------------------------------------
// User prompt response
// ---------------------------------------------------------------------------

async fn handle_prompt_response(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    prompt_id: &str,
    selected: Vec<usize>,
) {
    let pp = match pending_prompt.take() {
        Some(p) if p.prompt_id == prompt_id => p,
        other => {
            *pending_prompt = other;
            let _ = send_msg(socket, ServerMessage::Error {
                text: "no matching pending prompt".to_string(),
            }).await;
            return;
        }
    };

    // Build the tool output from the user's selection
    let selected_labels: Vec<String> = selected
        .iter()
        .filter_map(|&i| pp.options.get(i).cloned())
        .collect();

    let output = if pp.multi {
        if selected_labels.is_empty() {
            "User selected: (none)".to_string()
        } else {
            format!("User selected: {}", selected_labels.join(", "))
        }
    } else {
        match selected_labels.first() {
            Some(label) => format!("User selected: {label}"),
            None => "User selected: (none)".to_string(),
        }
    };

    let _ = send_msg(socket, ServerMessage::ToolOutput {
        tool_call_id: pp.tool_call.tool_call.id.clone(),
        output: output.clone(),
    }).await;
    append_tool_result(state, session_id, &output).await;
    *tool_depth += 1;

    // Continue the agentic loop
    if !tool_queue.is_empty() {
        present_next_tool(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    } else {
        invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    }
}

// ---------------------------------------------------------------------------
// Depth continuation prompt response
// ---------------------------------------------------------------------------

async fn handle_depth_prompt_response(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    selected: Vec<usize>,
) {
    let choice = selected.first().copied().unwrap_or(2); // default to "No"

    match choice {
        0 => {
            // "Yes" — continue, pause again after 25 more calls
            *depth_limit += state.config.max_tool_depth;
            let _ = send_msg(socket, ServerMessage::Notice {
                text: format!("Continuing. Will pause again after {} total calls.", *depth_limit),
            }).await;
            invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        }
        1 => {
            // "Yes, for next N" — continue, pause after N more calls, increment N
            *depth_limit += *bulk_increment;
            let _ = send_msg(socket, ServerMessage::Notice {
                text: format!("Continuing. Will pause again after {} total calls.", *depth_limit),
            }).await;
            *bulk_increment += 25;
            invoke_llm(state, session_id, socket, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        }
        _ => {
            // "No" — stop
            let _ = send_msg(socket, ServerMessage::Notice {
                text: "Stopped. Send a new message to continue.".to_string(),
            }).await;
        }
    }
}

fn truncate_tool_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let total_lines = output.bytes().filter(|&b| b == b'\n').count() + 1;
    let head_budget = max_chars * 60 / 100;
    let tail_budget = max_chars * 30 / 100;

    // Scan forward: collect head lines within budget
    let mut head_end = 0;
    let mut head_lines = 0;
    for line in output.lines() {
        let next = head_end + line.len() + 1; // +1 for newline
        if next > head_budget {
            break;
        }
        head_end = next;
        head_lines += 1;
    }

    // Scan backward: find tail start within budget
    let bytes = output.as_bytes();
    let mut tail_start = output.len();
    let mut tail_lines = 0;
    let mut pos = output.len();
    while pos > 0 {
        // Find the start of the previous line
        let line_end = pos;
        pos = if pos > 0 {
            bytes[..pos - 1].iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0)
        } else {
            0
        };
        let line_len = line_end - pos + 1;
        if (output.len() - pos) + line_len > tail_budget {
            break;
        }
        tail_start = pos;
        tail_lines += 1;
        if pos == 0 {
            break;
        }
    }

    let head = &output[..head_end];
    let tail = &output[tail_start..];

    format!(
        "{head}\n[… truncated — {total_lines} lines total, showing first {head_lines} and last {tail_lines} …]\n{tail}",
    )
}

async fn append_tool_result(state: &ServerState, session_id: Uuid, output: &str) {
    let truncated = truncate_tool_output(output, state.config.max_tool_output_chars);
    let mut sessions = state.sessions.write().await;
    if let Some(session) = sessions.get_mut(&session_id) {
        session.history.push(OllamaMessage {
            role: "user".to_string(),
            content: format!("[Tool output]:\n{truncated}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_output_unchanged() {
        let input = "line1\nline2\nline3";
        let result = truncate_tool_output(input, 8000);
        assert_eq!(result, input);
    }

    #[test]
    fn truncate_long_output_has_marker() {
        // Create output that exceeds the limit
        let lines: Vec<String> = (0..500).map(|i| format!("line {i}: some content here")).collect();
        let input = lines.join("\n");
        let result = truncate_tool_output(&input, 2000);
        assert!(result.contains("truncated"));
        assert!(result.len() < input.len());
    }

    #[test]
    fn truncate_preserves_head_and_tail() {
        let lines: Vec<String> = (0..100).map(|i| format!("L{i}")).collect();
        let input = lines.join("\n");
        let result = truncate_tool_output(&input, 200);
        // Should contain the first line and the last line
        assert!(result.contains("L0"));
        assert!(result.contains("L99"));
    }

    #[test]
    fn truncate_exact_boundary() {
        let input = "x".repeat(8000);
        let result = truncate_tool_output(&input, 8000);
        assert_eq!(result, input); // exactly at limit, no truncation
    }

    #[test]
    fn truncate_one_over_boundary() {
        let input = "x".repeat(8001);
        let result = truncate_tool_output(&input, 8000);
        assert!(result.contains("truncated"));
    }
}
