use std::collections::VecDeque;

use axum::extract::ws::{Message, WebSocket};
use bear_core::{ClientMessage, ProcessInfo, ServerMessage, ToolCall};
use futures::StreamExt;
use uuid::Uuid;

use crate::llm::{call_ollama_streaming, compact_history_if_needed, OllamaMessage};
use crate::process::{cleanup_session_processes, handle_process_kill, handle_process_input};
use crate::state::{PendingToolCall, ServerState};
use crate::tools::{execute_tool, parse_tool_calls};

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

/// Tracks a user_prompt_options tool that is waiting for the client's selection.
struct PendingPrompt {
    prompt_id: String,
    tool_call: PendingToolCall,
    options: Vec<String>,
    multi: bool,
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
                        handle_user_input(
                            &state, session_id, &mut socket,
                            &mut pending, &mut tool_queue, &mut tool_depth,
                            text,
                        ).await;
                    }
                    ClientMessage::ToolConfirm { tool_call_id, approved } => {
                        handle_tool_confirm(
                            &state, session_id, &mut socket,
                            &mut pending, &mut pending_prompt,
                            &mut tool_queue, &mut tool_depth,
                            &tool_call_id, approved,
                        ).await;
                    }
                    ClientMessage::UserPromptResponse { prompt_id, selected } => {
                        handle_prompt_response(
                            &state, session_id, &mut socket,
                            &mut pending, &mut pending_prompt,
                            &mut tool_queue, &mut tool_depth,
                            &prompt_id, selected,
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

async fn handle_user_input(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    text: String,
) {
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

    invoke_llm(state, session_id, socket, pending, tool_queue, tool_depth).await;
}

/// Call the LLM with current history, send assistant text, and queue all
/// tool calls for sequential user confirmation.
async fn invoke_llm(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
) {
    let limit = state.config.max_tool_depth;
    if *tool_depth >= limit {
        let _ = send_msg(socket, ServerMessage::Error {
            text: format!("Tool depth limit reached ({limit}). Send a new message to continue."),
        }).await;
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

    // Stream LLM response — send chunks to client as they arrive
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<String>(64);

    let http = state.http_client.clone();
    let cfg = state.config.clone();
    let history_clone = history.clone();
    let llm_handle = tokio::spawn(async move {
        call_ollama_streaming(&http, &cfg, &history_clone, &chunk_tx).await
    });

    // Forward chunks to the client as AssistantText
    while let Some(chunk) = chunk_rx.recv().await {
        let _ = send_msg(socket, ServerMessage::AssistantText {
            text: chunk,
        }).await;
    }

    let _ = send_msg(socket, ServerMessage::AssistantTextDone).await;

    match llm_handle.await {
        Ok(Ok(reply)) => {
            let tool_calls = parse_tool_calls(&reply.content);

            {
                let mut sessions = state.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.history.push(OllamaMessage {
                        role: "assistant".to_string(),
                        content: reply.content.clone(),
                    });
                }
            }

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
            present_next_tool(socket, pending, tool_queue).await;
        }
        Ok(Err(err)) => {
            let _ = send_msg(socket, ServerMessage::Error {
                text: format!("ollama request failed: {err}"),
            }).await;
        }
        Err(err) => {
            let _ = send_msg(socket, ServerMessage::Error {
                text: format!("LLM task panicked: {err}"),
            }).await;
        }
    }
}

/// Pop the next tool call from the queue and send a ToolRequest to the client.
async fn present_next_tool(
    socket: &mut WebSocket,
    pending: &mut Option<PendingToolCall>,
    tool_queue: &mut VecDeque<PendingToolCall>,
) {
    if let Some(ptc) = tool_queue.pop_front() {
        let _ = send_msg(socket, ServerMessage::ToolRequest {
            tool_call: ptc.tool_call.clone(),
        }).await;
        *pending = Some(ptc);
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
    pending_prompt: &mut Option<PendingPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
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
        invoke_llm(state, session_id, socket, pending, tool_queue, tool_depth).await;
        return;
    }

    // Special handling for user_prompt_options: send prompt to client and wait
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
                present_next_tool(socket, pending, tool_queue).await;
            } else {
                invoke_llm(state, session_id, socket, pending, tool_queue, tool_depth).await;
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

    let output = execute_tool(state, session_id, socket, &ptc).await;
    let _ = send_msg(socket, ServerMessage::ToolOutput {
        tool_call_id: ptc.tool_call.id.clone(),
        output: output.clone(),
    }).await;
    append_tool_result(state, session_id, &output).await;
    *tool_depth += 1;

    // If more tool calls queued from the same LLM response, present the next one
    if !tool_queue.is_empty() {
        present_next_tool(socket, pending, tool_queue).await;
    } else {
        // All tools from this response executed — re-invoke LLM
        invoke_llm(state, session_id, socket, pending, tool_queue, tool_depth).await;
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
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
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
        present_next_tool(socket, pending, tool_queue).await;
    } else {
        invoke_llm(state, session_id, socket, pending, tool_queue, tool_depth).await;
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
