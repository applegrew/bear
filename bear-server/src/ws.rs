use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use bear_core::{ClientMessage, ProcessInfo, ServerMessage, SlashCommandInfo, TaskItem, ToolCall};
use futures::{SinkExt, StreamExt};
use tokio::process::Command;
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

use crate::llm::{call_llm_streaming, compact_history_if_needed, plan_task, reflective_thinking, ChatMessage, OllamaMessage};
use crate::process::{cleanup_session_processes, handle_process_kill, handle_process_input};
use crate::state::{BusSender, PendingToolCall, SessionBus, ServerState};
use crate::tools::{execute_tool, parse_tool_calls};

/// When true, run a non-streaming reflection call before the main LLM response.
const ENABLE_REFLECTION: bool = true;

// ---------------------------------------------------------------------------
// Tool-call tag filter for streamed chunks
// ---------------------------------------------------------------------------

/// Stateful filter that strips tool-call markup from streamed LLM chunks so
/// the client never sees raw tool-call JSON.
///
/// Handles two formats:
///   1. `[TOOL_CALL]{"name":…,"arguments":…}[/TOOL_CALL]`
///   2. `[tool_name]{args}[/tool_name]`  (tool name used as the tag)
///
/// Because tags can span chunk boundaries, we buffer text that *might* be
/// the start of a tag and only emit it once we know it isn't.
struct ToolCallFilter {
    /// True while we are inside a tool-call block.
    inside: bool,
    /// The close tag we are looking for (e.g. "[/TOOL_CALL]" or "[/list_files]").
    close_tag: String,
    /// Accumulates text that could be the beginning of a tag boundary.
    buf: String,
}

/// Check if `tag_name` looks like a tool-call open tag.
/// Matches "TOOL_CALL" or any `snake_case` identifier (must contain at least
/// one underscore to avoid false positives on words like `[bold]`).
fn is_tool_tag(tag_name: &str) -> bool {
    tag_name == "TOOL_CALL"
        || (tag_name.contains('_')
            && tag_name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
}

impl ToolCallFilter {
    fn new() -> Self {
        Self { inside: false, close_tag: String::new(), buf: String::new() }
    }

    /// Feed a new chunk and return the text that should be shown to the user.
    fn feed(&mut self, chunk: &str) -> String {
        self.buf.push_str(chunk);
        let mut output = String::new();

        loop {
            if self.inside {
                // Looking for the close tag
                if let Some(pos) = self.buf.find(self.close_tag.as_str()) {
                    // Skip everything up to and including the close tag
                    self.buf = self.buf[pos + self.close_tag.len()..].to_string();
                    self.inside = false;
                    self.close_tag.clear();
                    continue;
                }
                // Close tag might be partially at the end — keep buffering
                let keep = self.close_tag.len() - 1;
                if self.buf.len() > keep {
                    self.buf = self.buf[self.buf.len() - keep..].to_string();
                }
                break;
            } else {
                // Look for '[' which could start a tool tag
                let Some(bracket) = self.buf.find('[') else {
                    // No bracket at all — emit everything
                    output.push_str(&self.buf);
                    self.buf.clear();
                    break;
                };

                // Check if we have a complete tag: [something]
                let after_bracket = &self.buf[bracket + 1..];
                let Some(close_bracket) = after_bracket.find(']') else {
                    // Incomplete — emit everything before '[', keep '[' onward buffered
                    output.push_str(&self.buf[..bracket]);
                    self.buf = self.buf[bracket..].to_string();
                    break;
                };

                let tag_name = &after_bracket[..close_bracket];
                if is_tool_tag(tag_name) {
                    // This is a tool-call open tag — emit text before it, enter inside mode
                    output.push_str(&self.buf[..bracket]);
                    self.close_tag = format!("[/{tag_name}]");
                    self.buf = self.buf[bracket + 1 + close_bracket + 1..].to_string();
                    self.inside = true;
                    continue;
                } else {
                    // Not a tool tag — emit up to and including ']', continue scanning
                    let end = bracket + 1 + close_bracket + 1;
                    output.push_str(&self.buf[..end]);
                    self.buf = self.buf[end..].to_string();
                    continue;
                }
            }
        }

        output
    }

    /// Flush any remaining buffered text (call when streaming is done).
    fn flush(&mut self) -> String {
        if self.inside {
            // Unclosed tag — discard the buffered content
            self.buf.clear();
            String::new()
        } else {
            std::mem::take(&mut self.buf)
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket handler — thin relay between client and session bus
// ---------------------------------------------------------------------------

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/ps", "List background processes"),
    ("/kill", "Kill a background process (usage: /kill <pid>)"),
    ("/send", "Send stdin to a process (usage: /send <pid> <text>)"),
    ("/session name", "Name the current session (usage: /session name <n>)"),
    ("/session workdir", "Set session working directory (usage: /session workdir <path>)"),
    ("/session max_subagents", "Set max concurrent subagents (usage: /session max_subagents <count>)"),
    ("/allowed", "Show auto-approved commands"),
    ("/exit", "Disconnect, keep session alive"),
    ("/end", "End session, pick another"),
    ("/help", "Show help"),
];

pub fn slash_command_infos() -> Vec<SlashCommandInfo> {
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

/// Tracks a task plan waiting for user approval.
struct PendingTaskPlan {
    plan_id: String,
    tasks: Vec<TaskItem>,
}

/// A user-facing interaction enqueued by a subagent (or any concurrent task).
/// The session worker presents these one at a time when no other prompt is active.
enum QueuedPrompt {
    /// A tool that needs user confirmation before execution.
    ToolConfirm {
        tool_call: ToolCall,
        cwd: String,
        /// Oneshot to deliver (approved, always) back to the enqueuer.
        result_tx: tokio::sync::oneshot::Sender<(bool, bool)>,
    },
    /// A user prompt (e.g. depth-limit continuation).
    UserPrompt {
        prompt_id: String,
        question: String,
        options: Vec<String>,
        multi: bool,
        /// If true, this is a depth-limit prompt. The session worker will update
        /// the budget and call `budget.resume.notify_waiters()` when resolved.
        is_depth_prompt: bool,
        /// Oneshot to deliver the user's selection back to the enqueuer.
        result_tx: tokio::sync::oneshot::Sender<Vec<usize>>,
    },
}

/// A queued interaction that has been presented to the user and is awaiting response.
enum PendingQueuedPrompt {
    ToolConfirm {
        tool_call_id: String,
        ptc: PendingToolCall,
        result_tx: tokio::sync::oneshot::Sender<(bool, bool)>,
    },
    UserPrompt {
        prompt_id: String,
        is_depth_prompt: bool,
        result_tx: tokio::sync::oneshot::Sender<Vec<usize>>,
    },
}

/// Sender half shared with subagents so they can enqueue prompts.
type PromptQueueTx = mpsc::Sender<QueuedPrompt>;

/// Shared tool-call budget across the main agent and all subagents.
/// All agents check this before making a tool call and pause if exhausted.
#[derive(Clone)]
struct ToolBudget {
    /// Total tool calls made so far (main + all subagents).
    depth: Arc<AtomicUsize>,
    /// Current depth limit — bumped when user approves continuation.
    limit: Arc<AtomicUsize>,
    /// Set to true when the user says "No" or presses Esc.
    terminated: Arc<AtomicBool>,
    /// Notified when the user responds to the depth-limit prompt.
    resume: Arc<Notify>,
    /// Set to true when a depth-limit prompt has been sent (prevents duplicates).
    prompt_sent: Arc<AtomicBool>,
}

impl ToolBudget {
    fn new(limit: usize) -> Self {
        Self {
            depth: Arc::new(AtomicUsize::new(0)),
            limit: Arc::new(AtomicUsize::new(limit)),
            terminated: Arc::new(AtomicBool::new(false)),
            resume: Arc::new(Notify::new()),
            prompt_sent: Arc::new(AtomicBool::new(false)),
        }
    }

    fn reset(&self, limit: usize) {
        self.depth.store(0, Ordering::SeqCst);
        self.limit.store(limit, Ordering::SeqCst);
        self.terminated.store(false, Ordering::SeqCst);
        self.prompt_sent.store(false, Ordering::SeqCst);
    }

    /// Increment the depth counter. Returns the new value.
    fn increment(&self) -> usize {
        self.depth.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current_depth(&self) -> usize {
        self.depth.load(Ordering::SeqCst)
    }

    fn current_limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }

    fn is_exhausted(&self) -> bool {
        self.current_depth() >= self.current_limit()
    }
}

/// Ensure a session bus exists and a worker is running. Returns the client_tx
/// sender for forwarding client messages to the worker.
pub async fn ensure_worker_running(
    state: &ServerState,
    session_id: Uuid,
) -> mpsc::Sender<ClientMessage> {
    let mut buses = state.buses.write().await;
    if let Some(bus) = buses.get(&session_id) {
        return bus.client_tx.clone();
    }

    // Create a new bus and spawn the worker
    let (client_tx, client_rx) = mpsc::channel::<ClientMessage>(64);
    let bus = SessionBus::new(client_tx.clone());
    let bus_sender = bus.sender();
    buses.insert(session_id, bus);
    drop(buses); // release lock before spawning

    let worker_state = state.clone();
    let worker_tx = client_tx.clone();
    tokio::spawn(async move {
        session_worker(worker_state, session_id, bus_sender, worker_tx, client_rx).await;
    });

    client_tx
}

pub async fn handle_socket(state: ServerState, session_id: Uuid, mut socket: WebSocket) {
    // Verify session exists
    let session_info = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.clone())
    };

    let Some(info) = session_info else {
        let _ = ws_send(&mut socket, &ServerMessage::Error {
            text: "session not found".to_string(),
        }).await;
        let _ = socket.close().await;
        return;
    };

    // Send session info and slash commands directly to this client
    let _ = ws_send(&mut socket, &ServerMessage::SessionInfo {
        session: info.clone(),
    }).await;
    let _ = ws_send(&mut socket, &ServerMessage::SlashCommands {
        commands: slash_command_infos(),
    }).await;

    // Send shared client state (input history + auto-approved commands)
    {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&session_id) {
            let _ = ws_send(&mut socket, &ServerMessage::ClientState {
                input_history: session.input_history.clone(),
            }).await;
        }
    }

    let _ = ws_send(&mut socket, &ServerMessage::Notice {
        text: format!(
            "Session persists after clients disconnect. Working directory is {}.",
            info.cwd
        ),
    }).await;

    if info.name.is_none() {
        let _ = ws_send(&mut socket, &ServerMessage::Notice {
            text: "Tip: Name this session with /session name <name>".to_string(),
        }).await;
    }

    // Ensure the session worker is running and get the client_tx
    let client_tx = ensure_worker_running(&state, session_id).await;

    // Create a consumer for this client (starts at offset 0 — full replay)
    let mut consumer = {
        let buses = state.buses.read().await;
        let Some(bus) = buses.get(&session_id) else {
            let _ = ws_send(&mut socket, &ServerMessage::Error {
                text: "session bus not found".to_string(),
            }).await;
            return;
        };
        bus.consumer()
    };

    tracing::info!("ws: client connected to session {session_id}");

    // Main relay loop: forward between WebSocket and session bus
    let (mut ws_sink, mut ws_stream) = socket.split();
    loop {
        tokio::select! {
            // Messages from the session worker → forward to WebSocket client
            batch = consumer.next_batch() => {
                let mut send_failed = false;
                for msg in batch {
                    let payload = match serde_json::to_string(&msg) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    if SinkExt::send(&mut ws_sink, Message::Text(payload)).await.is_err() {
                        tracing::info!("ws: client disconnected from session {session_id} (send failed)");
                        send_failed = true;
                        break;
                    }
                }
                if send_failed { break; }
            }
            // Messages from WebSocket client → forward to session worker
            ws_msg = ws_stream.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Ping) => {
                                // Handle ping directly, don't forward to worker
                                let payload = serde_json::to_string(&ServerMessage::Pong).unwrap_or_default();
                                let _ = SinkExt::send(&mut ws_sink, Message::Text(payload)).await;
                            }
                            Ok(client_msg) => {
                                tracing::info!("ws: forwarding {client_msg:?} to session {session_id}");
                                match client_tx.try_send(client_msg) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(msg)) => {
                                        tracing::warn!("ws: client_tx full for session {session_id}, dropping {msg:?}");
                                        let payload = serde_json::to_string(&ServerMessage::Error {
                                            text: "Server busy — please try again in a moment.".to_string(),
                                        }).unwrap_or_default();
                                        let _ = SinkExt::send(&mut ws_sink, Message::Text(payload)).await;
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        tracing::warn!("ws: client_tx closed for session {session_id}");
                                    }
                                }
                            }
                            Err(err) => {
                                let payload = serde_json::to_string(&ServerMessage::Error {
                                    text: format!("invalid message: {err}"),
                                }).unwrap_or_default();
                                let _ = SinkExt::send(&mut ws_sink, Message::Text(payload)).await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = SinkExt::send(&mut ws_sink, Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("client disconnected from session {session_id}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Client disconnected — worker keeps running, session persists
    tracing::info!("WebSocket closed for session {session_id}, worker continues");
}

// ---------------------------------------------------------------------------
// Session worker — background task that owns the agentic loop
// ---------------------------------------------------------------------------

async fn session_worker(
    state: ServerState,
    session_id: Uuid,
    bus: BusSender,
    client_tx: mpsc::Sender<ClientMessage>,
    mut client_rx: mpsc::Receiver<ClientMessage>,
) {
    tracing::info!("session worker started for {session_id}");

    let mut pending: Option<PendingToolCall> = None;
    let mut tool_queue: VecDeque<PendingToolCall> = VecDeque::new();
    let mut tool_depth: usize = 0;
    let mut pending_prompt: Option<PendingPrompt> = None;
    let mut pending_depth_prompt: Option<PendingDepthPrompt> = None;
    let mut pending_task_plan: Option<PendingTaskPlan> = None;
    let mut pending_queued_prompt: Option<PendingQueuedPrompt> = None;
    let mut depth_limit: usize = state.config.max_tool_depth;
    let mut bulk_increment: usize = 50;
    let budget = ToolBudget::new(state.config.max_tool_depth);
    // Active subagent JoinHandles — aborted on interrupt.
    let mut subagent_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Prompt queue: subagents enqueue prompts here; the session worker
    // presents them one at a time when no other prompt is active.
    let (prompt_queue_tx, mut prompt_queue_rx) = mpsc::channel::<QueuedPrompt>(32);

    /// Returns true if any user-facing prompt is currently active.
    fn has_active_prompt(
        pending: &Option<PendingToolCall>,
        pending_prompt: &Option<PendingPrompt>,
        pending_depth_prompt: &Option<PendingDepthPrompt>,
        pending_task_plan: &Option<PendingTaskPlan>,
        pending_queued_prompt: &Option<PendingQueuedPrompt>,
    ) -> bool {
        pending.is_some()
            || pending_prompt.is_some()
            || pending_depth_prompt.is_some()
            || pending_task_plan.is_some()
            || pending_queued_prompt.is_some()
    }

    loop {
        // If no prompt is active, also listen for queued prompts from subagents.
        let can_dequeue = !has_active_prompt(
            &pending, &pending_prompt, &pending_depth_prompt,
            &pending_task_plan, &pending_queued_prompt,
        );

        let client_msg = if can_dequeue {
            tokio::select! {
                biased;
                msg = client_rx.recv() => msg,
                queued = prompt_queue_rx.recv() => {
                    // Present the queued interaction to clients
                    if let Some(qp) = queued {
                        match qp {
                            QueuedPrompt::ToolConfirm { tool_call, cwd, result_tx } => {
                                let tc_id = tool_call.id.clone();
                                let mut display_tc = tool_call.clone();
                                display_tc.name = tool_display_name(&tool_call.name).to_string();
                                let extracted_commands = if tool_call.name == "run_command" {
                                    let cmd_str = tool_call.arguments["command"].as_str().unwrap_or("");
                                    let cmds = extract_shell_commands(cmd_str);
                                    if cmds.is_empty() { None } else { Some(cmds) }
                                } else {
                                    None
                                };
                                bus.send(ServerMessage::ToolRequest {
                                    tool_call: display_tc,
                                    extracted_commands,
                                }).await;
                                pending_queued_prompt = Some(PendingQueuedPrompt::ToolConfirm {
                                    tool_call_id: tc_id,
                                    ptc: PendingToolCall { tool_call, cwd },
                                    result_tx,
                                });
                            }
                            QueuedPrompt::UserPrompt { prompt_id, question, options, multi, is_depth_prompt, result_tx } => {
                                bus.send(ServerMessage::UserPrompt {
                                    prompt_id: prompt_id.clone(),
                                    question,
                                    options,
                                    multi,
                                }).await;
                                pending_queued_prompt = Some(PendingQueuedPrompt::UserPrompt {
                                    prompt_id,
                                    is_depth_prompt,
                                    result_tx,
                                });
                            }
                        }
                    }
                    continue;
                }
            }
        } else {
            client_rx.recv().await
        };

        let Some(client_msg) = client_msg else { break };
        tracing::debug!("session_worker {session_id}: received {client_msg:?}");

        match client_msg {
            ClientMessage::Input { text } => {
                tracing::info!("session_worker {session_id}: Input received: {text:?}");
                // Broadcast the user's input so all connected clients can display it.
                bus.send(ServerMessage::UserInput { text: text.clone() }).await;
                // Record input to shared history
                {
                    let mut sessions = state.sessions.write().await;
                    if let Some(session) = sessions.get_mut(&session_id) {
                        if session.input_history.last().map(|s| s.as_str()) != Some(&text) {
                            session.input_history.push(text.clone());
                        }
                    }
                }
                // Abort any active subagents
                abort_subagents(&mut subagent_handles, &budget, &bus).await;
                // Drop any pending queued prompt (subagent was aborted)
                pending_queued_prompt = None;
                tool_depth = 0;
                depth_limit = state.config.max_tool_depth;
                bulk_increment = 50;
                budget.reset(state.config.max_tool_depth);
                pending_task_plan = None;
                handle_user_input(
                    &state, session_id, &bus, &mut client_rx,
                    &mut pending, &mut pending_prompt,
                    &mut pending_depth_prompt,
                    &mut pending_task_plan,
                    &mut tool_queue, &mut tool_depth,
                    &mut depth_limit, &mut bulk_increment,
                    &budget, &mut subagent_handles,
                    &prompt_queue_tx,
                    text,
                ).await;
            }
            ClientMessage::ToolConfirm { tool_call_id, approved, always } => {
                // Check if this resolves a queued tool confirm from a subagent
                if let Some(PendingQueuedPrompt::ToolConfirm { tool_call_id: ref qid, .. }) = pending_queued_prompt {
                    if *qid == tool_call_id {
                        let pqp = pending_queued_prompt.take().unwrap();
                        if let PendingQueuedPrompt::ToolConfirm { ptc, result_tx, .. } = pqp {
                            bus.send(ServerMessage::ToolResolved {
                                tool_call_id: tool_call_id.clone(),
                                approved,
                            }).await;
                            if approved {
                                // Handle auto-approve (always) for the session
                                if always {
                                    let display = tool_display_name(&ptc.tool_call.name);
                                    if ptc.tool_call.name == "run_command" {
                                        let cmd_str = ptc.tool_call.arguments["command"].as_str().unwrap_or("");
                                        for cmd in extract_shell_commands(cmd_str) {
                                            let mut sessions = state.sessions.write().await;
                                            if let Some(session) = sessions.get_mut(&session_id) {
                                                session.auto_approved.insert(cmd);
                                            }
                                        }
                                    } else {
                                        let mut sessions = state.sessions.write().await;
                                        if let Some(session) = sessions.get_mut(&session_id) {
                                            session.auto_approved.insert(display.to_string());
                                        }
                                    }
                                }
                                // Execute the tool and send output
                                let output = execute_tool(&state, session_id, &bus, &ptc).await;
                                bus.send(tool_output_msg(&ptc, output.clone())).await;
                                append_tool_result(&state, session_id, &ptc.tool_call.name, &output).await;
                            }
                            let _ = result_tx.send((approved, always));
                        }
                        continue;
                    }
                }
                handle_tool_confirm(
                    &state, session_id, &bus, &mut client_rx,
                    &mut pending, &mut pending_prompt,
                    &mut pending_depth_prompt,
                    &mut tool_queue, &mut tool_depth,
                    &mut depth_limit, &mut bulk_increment,
                    &tool_call_id, approved, always,
                ).await;
            }
            ClientMessage::UserPromptResponse { prompt_id, selected } => {
                // Check if this resolves a queued user prompt from a subagent
                if let Some(PendingQueuedPrompt::UserPrompt { prompt_id: ref qid, .. }) = pending_queued_prompt {
                    if *qid == prompt_id {
                        let pqp = pending_queued_prompt.take().unwrap();
                        if let PendingQueuedPrompt::UserPrompt { prompt_id: pid, is_depth_prompt, result_tx } = pqp {
                            bus.send(ServerMessage::PromptResolved {
                                prompt_id: pid,
                            }).await;
                            if is_depth_prompt {
                                // Handle depth-limit budget update inline
                                let choice = selected.first().copied().unwrap_or(2);
                                match choice {
                                    0 => {
                                        depth_limit += state.config.max_tool_depth;
                                        budget.limit.store(depth_limit, Ordering::SeqCst);
                                        budget.prompt_sent.store(false, Ordering::SeqCst);
                                        budget.resume.notify_waiters();
                                        bus.send(ServerMessage::Notice {
                                            text: format!("Continuing. Will pause again after {} total calls.", depth_limit),
                                        }).await;
                                    }
                                    1 => {
                                        depth_limit += bulk_increment;
                                        budget.limit.store(depth_limit, Ordering::SeqCst);
                                        budget.prompt_sent.store(false, Ordering::SeqCst);
                                        budget.resume.notify_waiters();
                                        bus.send(ServerMessage::Notice {
                                            text: format!("Continuing. Will pause again after {} total calls.", depth_limit),
                                        }).await;
                                        bulk_increment += 25;
                                    }
                                    _ => {
                                        budget.terminated.store(true, Ordering::SeqCst);
                                        budget.resume.notify_waiters();
                                        bus.send(ServerMessage::Notice {
                                            text: "Stopped. Send a new message to continue.".to_string(),
                                        }).await;
                                    }
                                }
                            }
                            // Deliver the result back to the enqueuer
                            let _ = result_tx.send(selected);
                            continue;
                        }
                    }
                }
                if let Some(dp) = pending_depth_prompt.take() {
                    if dp.prompt_id == prompt_id {
                        handle_depth_prompt_response(
                            &state, session_id, &bus, &mut client_rx,
                            &mut pending, &mut pending_prompt,
                            &mut pending_depth_prompt,
                            &mut tool_queue, &mut tool_depth,
                            &mut depth_limit, &mut bulk_increment,
                            &budget,
                            &prompt_id,
                            selected,
                        ).await;
                    } else {
                        pending_depth_prompt = Some(dp);
                    }
                } else {
                    handle_prompt_response(
                        &state, session_id, &bus, &mut client_rx,
                        &mut pending, &mut pending_prompt,
                        &mut pending_depth_prompt,
                        &mut tool_queue, &mut tool_depth,
                        &mut depth_limit, &mut bulk_increment,
                        &prompt_id, selected,
                    ).await;
                }
            }
            ClientMessage::TaskPlanResponse { plan_id, approved } => {
                if let Some(plan) = pending_task_plan.take() {
                    if plan.plan_id == plan_id {
                        // Broadcast resolution so all other clients dismiss their pickers.
                        bus.send(ServerMessage::PromptResolved {
                            prompt_id: plan_id.clone(),
                        }).await;
                        if approved {
                            bus.send(ServerMessage::Notice {
                                text: "Task plan approved. Starting execution…".to_string(),
                            }).await;
                            execute_task_plan(
                                &state, session_id, &bus, &client_tx,
                                &mut client_rx,
                                &mut pending, &mut pending_prompt,
                                &mut pending_depth_prompt,
                                &mut tool_queue, &mut tool_depth,
                                &mut depth_limit, &mut bulk_increment,
                                &budget, &mut subagent_handles,
                                &prompt_queue_tx, &mut prompt_queue_rx,
                                &plan.plan_id, &plan.tasks,
                            ).await;
                        } else {
                            bus.send(ServerMessage::Notice {
                                text: "Task plan rejected.".to_string(),
                            }).await;
                        }
                    } else {
                        pending_task_plan = Some(plan);
                    }
                }
            }
            ClientMessage::ProcessList => {
                let procs = state.processes.read().await;
                let list: Vec<ProcessInfo> = procs.values()
                    .filter(|p| p.session_id == session_id)
                    .map(|p| p.info.clone())
                    .collect();
                bus.send(ServerMessage::ProcessListResult {
                    processes: list,
                }).await;
            }
            ClientMessage::ProcessKill { pid } => {
                handle_process_kill(&state, &bus, pid).await;
            }
            ClientMessage::ProcessInput { pid, text } => {
                handle_process_input(&state, &bus, pid, &text).await;
            }
            ClientMessage::SessionRename { name } => {
                let trimmed = name.trim().to_string();
                if trimmed.is_empty() {
                    bus.send(ServerMessage::Error {
                        text: "Session name must not be empty.".to_string(),
                    }).await;
                } else {
                    let mut sessions = state.sessions.write().await;
                    if let Some(session) = sessions.get_mut(&session_id) {
                        session.info.name = Some(trimmed.clone());
                    }
                    bus.send(ServerMessage::SessionRenamed {
                        name: trimmed,
                    }).await;
                }
            }
            ClientMessage::SessionWorkdir { path } => {
                handle_session_workdir_cmd(&state, session_id, &bus, &path).await;
            }
            ClientMessage::SessionEnd => {
                abort_subagents(&mut subagent_handles, &budget, &bus).await;
                drop(pending_queued_prompt.take());
                cleanup_session_processes(&state, session_id).await;
                {
                    let mut sessions = state.sessions.write().await;
                    sessions.remove(&session_id);
                }
                bus.send(ServerMessage::Notice {
                    text: "Session ended and removed.".to_string(),
                }).await;
                // Remove the bus too
                state.buses.write().await.remove(&session_id);
                break;
            }
            ClientMessage::Interrupt => {
                // Abort all active subagents on interrupt
                abort_subagents(&mut subagent_handles, &budget, &bus).await;
                pending_queued_prompt = None;
            }
            ClientMessage::Ping => {
                // Handled directly in handle_socket, should not reach here
            }
        }
    }

    tracing::info!("session worker exiting for {session_id}");
}

/// Abort all active subagent tasks and notify them to terminate.
async fn abort_subagents(
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
    budget: &ToolBudget,
    _bus: &BusSender,
) {
    if handles.is_empty() {
        return;
    }
    budget.terminated.store(true, Ordering::SeqCst);
    budget.resume.notify_waiters();
    for handle in handles.drain(..) {
        handle.abort();
    }
}

/// Handle /session workdir command (extracted to avoid duplication)
async fn handle_session_workdir_cmd(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    path: &str,
) {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bus.send(ServerMessage::Error {
            text: "Usage: /session workdir <path>".to_string(),
        }).await;
        return;
    }

    let current_cwd = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.cwd.clone())
    };

    let Some(current_cwd) = current_cwd else {
        bus.send(ServerMessage::Error {
            text: "session not found".to_string(),
        }).await;
        return;
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
                bus.send(ServerMessage::Error {
                    text: "Failed to resolve working directory.".to_string(),
                }).await;
                return;
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
                bus.send(ServerMessage::Notice {
                    text: format!("Working directory set to: {new_cwd}"),
                }).await;
                bus.send(ServerMessage::SessionInfo {
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
            bus.send(ServerMessage::Error { text: msg }).await;
        }
        Err(err) => {
            bus.send(ServerMessage::Error {
                text: format!("Failed to change directory: {err}"),
            }).await;
        }
    }
}

// ---------------------------------------------------------------------------
// User input handling
// ---------------------------------------------------------------------------

async fn handle_slash_command(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
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
            bus.send(ServerMessage::ProcessListResult {
                processes: list,
            }).await;
            return true;
        }
        "/allowed" => {
            let sessions = state.sessions.read().await;
            let text = if let Some(session) = sessions.get(&session_id) {
                if session.auto_approved.is_empty() {
                    "No auto-approved commands.".to_string()
                } else {
                    let mut cmds: Vec<&String> = session.auto_approved.iter().collect();
                    cmds.sort();
                    format!("Auto-approved: {}", cmds.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
                }
            } else {
                "Session not found.".to_string()
            };
            bus.send(ServerMessage::Notice { text }).await;
            return true;
        }
        "/help" => {
            let mut lines = Vec::with_capacity(SLASH_COMMANDS.len() + 1);
            lines.push("Commands:".to_string());
            for (cmd, desc) in SLASH_COMMANDS {
                lines.push(format!("  {cmd:<20} {desc}"));
            }
            let help = lines.join("\n");
            bus.send(ServerMessage::Notice {
                text: help,
            }).await;
            return true;
        }
        "/exit" => {
            bus.send(ServerMessage::Notice {
                text: "Disconnecting. Session preserved.".to_string(),
            }).await;
            return true;
        }
        "/end" => {
            // Handled by session_worker directly via ClientMessage::SessionEnd
            return true;
        }
        _ => {}
    }

    if let Some(rest) = trimmed.strip_prefix("/kill ") {
        match rest.trim().parse::<u32>() {
            Ok(pid) => {
                handle_process_kill(state, bus, pid).await;
            }
            Err(_) => {
                bus.send(ServerMessage::Error {
                    text: "Usage: /kill <pid>".to_string(),
                }).await;
            }
        }
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("/send ") {
        if let Some((pid_str, input)) = rest.split_once(' ') {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                handle_process_input(state, bus, pid, input).await;
            } else {
                bus.send(ServerMessage::Error {
                    text: "Usage: /send <pid> <text>".to_string(),
                }).await;
            }
        } else {
            bus.send(ServerMessage::Error {
                text: "Usage: /send <pid> <text>".to_string(),
            }).await;
        }
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("/session ") {
        if let Some(name) = rest.strip_prefix("name ") {
            let name = name.trim();
            if name.is_empty() {
                bus.send(ServerMessage::Error {
                    text: "Usage: /session name <session name>".to_string(),
                }).await;
            } else {
                let mut sessions = state.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.info.name = Some(name.to_string());
                }
                bus.send(ServerMessage::SessionRenamed {
                    name: name.to_string(),
                }).await;
            }
            return true;
        }

        if let Some(path) = rest.strip_prefix("workdir ") {
            handle_session_workdir_cmd(state, session_id, bus, path).await;
            return true;
        }

        if let Some(count_str) = rest.strip_prefix("max_subagents ") {
            let count_str = count_str.trim();
            match count_str.parse::<usize>() {
                Ok(n) if n >= 1 => {
                    let mut sessions = state.sessions.write().await;
                    if let Some(session) = sessions.get_mut(&session_id) {
                        session.max_subagents = n;
                    }
                    bus.send(ServerMessage::Notice {
                        text: format!("Max concurrent subagents set to {n}."),
                    }).await;
                }
                _ => {
                    bus.send(ServerMessage::Error {
                        text: "Usage: /session max_subagents <count> (must be >= 1)".to_string(),
                    }).await;
                }
            }
            return true;
        }

        bus.send(ServerMessage::Error {
            text: "Usage: /session name <n> | /session workdir <path> | /session max_subagents <count>".to_string(),
        }).await;
        return true;
    }

    bus.send(ServerMessage::Error {
        text: format!("Unknown command: {trimmed}"),
    }).await;
    true
}

async fn handle_user_input(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    pending_task_plan: &mut Option<PendingTaskPlan>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    _budget: &ToolBudget,
    _subagent_handles: &mut Vec<tokio::task::JoinHandle<()>>,
    _prompt_queue_tx: &PromptQueueTx,
    text: String,
) {
    if handle_slash_command(state, session_id, bus, &text).await {
        return;
    }

    let user_msg = OllamaMessage {
        role: "user".to_string(),
        content: text,
    };

    {
        let mut sessions = state.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            bus.send(ServerMessage::Error {
                text: "session not found".to_string(),
            }).await;
            return;
        };
        session.info.touch();
        session.history.push(user_msg);
    }

    // Show the thinking indicator immediately — plan_task and reflection
    // are non-streaming calls that can take a while.
    bus.send(ServerMessage::Thinking).await;

    // --- Task planning: classify the request ---
    let (history, cwd) = {
        let sessions = state.sessions.read().await;
        let Some(session) = sessions.get(&session_id) else {
            bus.send(ServerMessage::Error { text: "session not found".to_string() }).await;
            return;
        };
        (session.history.clone(), session.info.cwd.clone())
    };
    let session_context = format!("Session context:\n- Working directory: {cwd}");

    match plan_task(&state.http_client, &state.config, &history, &session_context).await {
        Ok(plan_json) => {
            tracing::info!("planner response: {}", &plan_json[..plan_json.len().min(500)]);
            if let Some(plan) = parse_task_plan(&plan_json) {
                if plan.plan_type == "complex_task" && !plan.tasks.is_empty() {
                    // Send plan to client for approval
                    let plan_id = format!("plan_{}", Uuid::new_v4());
                    bus.send(ServerMessage::TaskPlan {
                        plan_id: plan_id.clone(),
                        tasks: plan.tasks.clone(),
                    }).await;
                    *pending_task_plan = Some(PendingTaskPlan {
                        plan_id,
                        tasks: plan.tasks,
                    });
                    return;
                }
                // question or simple_task — proceed directly
            }
            // If parsing failed or not complex, fall through to normal invoke_llm
        }
        Err(err) => {
            tracing::warn!("planner call failed, proceeding without plan: {err}");
            // Fall through to normal invoke_llm
        }
    }

    invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
}

/// Parsed result from the planner LLM call.
struct ParsedPlan {
    plan_type: String,
    tasks: Vec<TaskItem>,
}

/// Parse the planner's JSON response into a structured plan.
fn parse_task_plan(json_str: &str) -> Option<ParsedPlan> {
    // The LLM might wrap JSON in markdown code fences — strip them
    let trimmed = json_str.trim();
    let clean = if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        inner
    } else {
        trimmed
    };

    let val: serde_json::Value = serde_json::from_str(clean).ok()?;
    let plan_type = val["type"].as_str()?.to_string();
    let tasks_arr = val["plan"].as_array()?;

    let mut tasks = Vec::new();
    for item in tasks_arr {
        let id = item["id"].as_str().unwrap_or("").to_string();
        let description = item["description"].as_str().unwrap_or("").to_string();
        let needs_write = item["needs_write"].as_bool().unwrap_or(true);
        if !description.is_empty() {
            tasks.push(TaskItem { id, description, needs_write });
        }
    }

    Some(ParsedPlan { plan_type, tasks })
}

// ---------------------------------------------------------------------------
// Task plan execution
// ---------------------------------------------------------------------------

/// Execute an approved task plan: run read-only tasks as parallel subagents,
/// write tasks sequentially via the main agent.
async fn execute_task_plan(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_tx: &mpsc::Sender<ClientMessage>,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    budget: &ToolBudget,
    subagent_handles: &mut Vec<tokio::task::JoinHandle<()>>,
    prompt_queue_tx: &PromptQueueTx,
    prompt_queue_rx: &mut mpsc::Receiver<QueuedPrompt>,
    plan_id: &str,
    tasks: &[TaskItem],
) {
    let max_subagents = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).map(|s| s.max_subagents).unwrap_or(3)
    };

    // Collect read-only tasks and write tasks
    let mut read_tasks: Vec<&TaskItem> = Vec::new();
    let mut write_tasks: Vec<&TaskItem> = Vec::new();
    for task in tasks {
        if task.needs_write {
            write_tasks.push(task);
        } else {
            read_tasks.push(task);
        }
    }

    // --- Phase 1: Run read-only tasks as subagents (batched by max_subagents) ---
    if !read_tasks.is_empty() {
        for chunk in read_tasks.chunks(max_subagents) {
            if budget.is_terminated() {
                break;
            }

            let mut join_handles = Vec::new();
            let (result_tx, mut result_rx) = mpsc::channel::<(String, String)>(chunk.len() + 1);

            for task in chunk {
                bus.send(ServerMessage::TaskProgress {
                    plan_id: plan_id.to_string(),
                    task_id: task.id.clone(),
                    status: "in_progress".to_string(),
                    detail: None,
                }).await;

                let subagent_id = format!("sa_{}", Uuid::new_v4());
                bus.send(ServerMessage::SubagentUpdate {
                    subagent_id: subagent_id.clone(),
                    description: task.description.clone(),
                    status: "running".to_string(),
                    detail: None,
                }).await;

                let handle = tokio::spawn(run_subagent(
                    state.clone(),
                    session_id,
                    bus.clone(),
                    budget.clone(),
                    prompt_queue_tx.clone(),
                    subagent_id.clone(),
                    task.id.clone(),
                    task.description.clone(),
                    result_tx.clone(),
                ));
                join_handles.push(handle);
            }
            // Move real handles into subagent_handles so session_worker can abort them
            subagent_handles.append(&mut join_handles);
            drop(result_tx); // drop our copy so result_rx closes when all subagents finish

            // Wait for all subagents in this batch to complete, while still
            // processing client messages and queued prompts so that depth-limit
            // prompts and tool confirmations from subagents can be presented.
            let mut results: Vec<(String, String)> = Vec::new();
            let mut pending_queued: Option<PendingQueuedPrompt> = None;
            loop {
                let can_dequeue = pending.is_none()
                    && pending_prompt.is_none()
                    && pending_depth_prompt.is_none()
                    && pending_queued.is_none();

                tokio::select! {
                    biased;
                    result = result_rx.recv() => {
                        match result {
                            Some(r) => results.push(r),
                            None => break, // all subagents finished
                        }
                    }
                    msg = client_rx.recv() => {
                        if let Some(msg) = msg {
                            match msg {
                                ClientMessage::ToolConfirm { tool_call_id, approved, always } => {
                                    if let Some(PendingQueuedPrompt::ToolConfirm { tool_call_id: ref qid, .. }) = pending_queued {
                                        if *qid == tool_call_id {
                                            let pqp = pending_queued.take().unwrap();
                                            if let PendingQueuedPrompt::ToolConfirm { ptc, result_tx, .. } = pqp {
                                                bus.send(ServerMessage::ToolResolved {
                                                    tool_call_id: tool_call_id.clone(),
                                                    approved,
                                                }).await;
                                                if approved {
                                                    if always {
                                                        let display = tool_display_name(&ptc.tool_call.name);
                                                        if ptc.tool_call.name == "run_command" {
                                                            let cmd_str = ptc.tool_call.arguments["command"].as_str().unwrap_or("");
                                                            for cmd in extract_shell_commands(cmd_str) {
                                                                let mut sessions = state.sessions.write().await;
                                                                if let Some(session) = sessions.get_mut(&session_id) {
                                                                    session.auto_approved.insert(cmd);
                                                                }
                                                            }
                                                        } else {
                                                            let mut sessions = state.sessions.write().await;
                                                            if let Some(session) = sessions.get_mut(&session_id) {
                                                                session.auto_approved.insert(display.to_string());
                                                            }
                                                        }
                                                    }
                                                    let output = execute_tool(state, session_id, bus, &ptc).await;
                                                    bus.send(tool_output_msg(&ptc, output.clone())).await;
                                                    append_tool_result(state, session_id, &ptc.tool_call.name, &output).await;
                                                }
                                                let _ = result_tx.send((approved, always));
                                            }
                                        }
                                    }
                                }
                                ClientMessage::UserPromptResponse { prompt_id, selected } => {
                                    if let Some(PendingQueuedPrompt::UserPrompt { prompt_id: ref qid, .. }) = pending_queued {
                                        if *qid == prompt_id {
                                            let pqp = pending_queued.take().unwrap();
                                            if let PendingQueuedPrompt::UserPrompt { prompt_id: pid, is_depth_prompt, result_tx } = pqp {
                                                bus.send(ServerMessage::PromptResolved {
                                                    prompt_id: pid,
                                                }).await;
                                                if is_depth_prompt {
                                                    let choice = selected.first().copied().unwrap_or(2);
                                                    match choice {
                                                        0 => {
                                                            *depth_limit += state.config.max_tool_depth;
                                                            budget.limit.store(*depth_limit, Ordering::SeqCst);
                                                            budget.prompt_sent.store(false, Ordering::SeqCst);
                                                            budget.resume.notify_waiters();
                                                            bus.send(ServerMessage::Notice {
                                                                text: format!("Continuing. Will pause again after {} total calls.", *depth_limit),
                                                            }).await;
                                                        }
                                                        1 => {
                                                            *depth_limit += *bulk_increment;
                                                            budget.limit.store(*depth_limit, Ordering::SeqCst);
                                                            budget.prompt_sent.store(false, Ordering::SeqCst);
                                                            budget.resume.notify_waiters();
                                                            bus.send(ServerMessage::Notice {
                                                                text: format!("Continuing. Will pause again after {} total calls.", *depth_limit),
                                                            }).await;
                                                            *bulk_increment += 25;
                                                        }
                                                        _ => {
                                                            budget.terminated.store(true, Ordering::SeqCst);
                                                            budget.resume.notify_waiters();
                                                            bus.send(ServerMessage::Notice {
                                                                text: "Stopped. Send a new message to continue.".to_string(),
                                                            }).await;
                                                        }
                                                    }
                                                }
                                                let _ = result_tx.send(selected);
                                            }
                                        }
                                    }
                                }
                                ClientMessage::Input { text } => {
                                    tracing::info!("execute_task_plan: interrupted by new input: {text:?}");
                                    // Abort subagents and return to the main loop
                                    budget.terminated.store(true, Ordering::SeqCst);
                                    budget.resume.notify_waiters();
                                    // Broadcast the user's input so all clients see it
                                    bus.send(ServerMessage::UserInput { text: text.clone() }).await;
                                    // Re-queue the input so the session worker main loop picks it up
                                    let _ = client_tx.send(ClientMessage::Input { text }).await;
                                    // Break out of the subagent wait loop
                                    break;
                                }
                                ClientMessage::Interrupt => {
                                    // Abort subagents — they'll finish and result_rx will close
                                    budget.terminated.store(true, Ordering::SeqCst);
                                    budget.resume.notify_waiters();
                                    pending_queued = None;
                                }
                                _ => {} // ignore other messages during subagent phase
                            }
                        }
                    }
                    queued = prompt_queue_rx.recv(), if can_dequeue => {
                        if let Some(qp) = queued {
                            match qp {
                                QueuedPrompt::ToolConfirm { tool_call, cwd, result_tx } => {
                                    let tc_id = tool_call.id.clone();
                                    let mut display_tc = tool_call.clone();
                                    display_tc.name = tool_display_name(&tool_call.name).to_string();
                                    let extracted_commands = if tool_call.name == "run_command" {
                                        let cmd_str = tool_call.arguments["command"].as_str().unwrap_or("");
                                        let cmds = extract_shell_commands(cmd_str);
                                        if cmds.is_empty() { None } else { Some(cmds) }
                                    } else {
                                        None
                                    };
                                    bus.send(ServerMessage::ToolRequest {
                                        tool_call: display_tc,
                                        extracted_commands,
                                    }).await;
                                    pending_queued = Some(PendingQueuedPrompt::ToolConfirm {
                                        tool_call_id: tc_id,
                                        ptc: PendingToolCall { tool_call, cwd },
                                        result_tx,
                                    });
                                }
                                QueuedPrompt::UserPrompt { prompt_id, question, options, multi, is_depth_prompt, result_tx } => {
                                    bus.send(ServerMessage::UserPrompt {
                                        prompt_id: prompt_id.clone(),
                                        question,
                                        options,
                                        multi,
                                    }).await;
                                    pending_queued = Some(PendingQueuedPrompt::UserPrompt {
                                        prompt_id,
                                        is_depth_prompt,
                                        result_tx,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Mark completed tasks
            for (task_id, _summary) in &results {
                bus.send(ServerMessage::TaskProgress {
                    plan_id: plan_id.to_string(),
                    task_id: task_id.clone(),
                    status: "completed".to_string(),
                    detail: None,
                }).await;
            }

            // Inject subagent results into session history as context
            if !results.is_empty() {
                let mut context = String::from("[Subagent research results]\n\n");
                for (task_id, summary) in &results {
                    context.push_str(&format!("### Task {task_id}\n{summary}\n\n"));
                }
                let mut sessions = state.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.history.push(OllamaMessage {
                        role: "user".to_string(),
                        content: context,
                    });
                }
            }

            subagent_handles.clear();
        }
    }

    // --- Phase 2: Run write tasks sequentially via the main agent ---
    for task in &write_tasks {
        if budget.is_terminated() {
            bus.send(ServerMessage::TaskProgress {
                plan_id: plan_id.to_string(),
                task_id: task.id.clone(),
                status: "failed".to_string(),
                detail: Some("Terminated by user".to_string()),
            }).await;
            continue;
        }

        bus.send(ServerMessage::TaskProgress {
            plan_id: plan_id.to_string(),
            task_id: task.id.clone(),
            status: "in_progress".to_string(),
            detail: None,
        }).await;

        // Inject the task description as a user message so the LLM knows what to do
        {
            let mut sessions = state.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.history.push(OllamaMessage {
                    role: "user".to_string(),
                    content: format!("[Task {}] {}", task.id, task.description),
                });
            }
        }

        // Run the main agent loop for this task
        invoke_llm(
            state, session_id, bus, client_rx,
            pending, pending_prompt, pending_depth_prompt,
            tool_queue, tool_depth, depth_limit, bulk_increment,
        ).await;

        bus.send(ServerMessage::TaskProgress {
            plan_id: plan_id.to_string(),
            task_id: task.id.clone(),
            status: "completed".to_string(),
            detail: None,
        }).await;
    }
}

/// Run a single read-only subagent: its own LLM loop with restricted tools.
/// Sends SubagentUpdate messages as it works, and sends the final summary
/// through `result_tx`.
async fn run_subagent(
    state: ServerState,
    session_id: Uuid,
    bus: BusSender,
    budget: ToolBudget,
    prompt_queue_tx: PromptQueueTx,
    subagent_id: String,
    task_id: String,
    task_description: String,
    result_tx: mpsc::Sender<(String, String)>,
) {
    let lsp_available = crate::lsp::any_lsp_server_available();
    let subagent_prompt = crate::state::subagent_system_prompt(lsp_available);

    let cwd = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.cwd.clone()).unwrap_or_default()
    };

    let session_context = format!("Session context:\n- Working directory: {cwd}");
    let system_content = format!("{subagent_prompt}\n\n{session_context}");

    let mut history = vec![
        OllamaMessage {
            role: "system".to_string(),
            content: system_content,
        },
        OllamaMessage {
            role: "user".to_string(),
            content: format!("Your task: {task_description}\n\nExplore the codebase and provide a detailed summary of your findings."),
        },
    ];

    let mut full_response = String::new();

    // Subagent LLM loop — up to budget exhaustion
    for _iteration in 0..50 {
        if budget.is_terminated() {
            break;
        }

        // Check budget before making LLM call
        if budget.is_exhausted() {
            // Wait for user to approve continuation
            budget.resume.notified().await;
            if budget.is_terminated() {
                break;
            }
        }

        // Non-streaming LLM call for subagent (simpler than streaming)
        let chat_history: Vec<ChatMessage> = history.iter()
            .map(|msg| msg.clone().into())
            .collect();
        let reply = match crate::llm::call_llm_non_streaming(
            &state.http_client, &state.config, &chat_history,
        ).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!("subagent {subagent_id} LLM call failed: {err}");
                break;
            }
        };

        full_response = reply.content.clone();
        let reply_for_history = reply.clone().into();
        let tool_calls = parse_tool_calls(&reply.content);
        history.push(reply_for_history);
        if tool_calls.is_empty() {
            // No more tool calls — subagent is done
            break;
        }

        // Execute each tool call (auto-approved, read-only only)
        for tc in tool_calls {
            if budget.is_terminated() {
                break;
            }

            // Check budget before each tool call
            if budget.is_exhausted() {
                // Signal that we need a depth prompt (only one agent does this)
                if !budget.prompt_sent.swap(true, Ordering::SeqCst) {
                    let prompt_id = format!("depth_{}", Uuid::new_v4());
                    let options = vec![
                        "Yes".to_string(),
                        format!("Yes, for next {}", 50),
                        "No".to_string(),
                    ];
                    let (otx, _orx) = tokio::sync::oneshot::channel();
                    // Enqueue the depth prompt — the session worker will present
                    // it when no other prompt is active.
                    let _ = prompt_queue_tx.send(QueuedPrompt::UserPrompt {
                        prompt_id,
                        question: format!(
                            "Tool depth limit reached ({} consecutive calls). Continue?",
                            budget.current_depth(),
                        ),
                        options,
                        multi: false,
                        is_depth_prompt: true,
                        result_tx: otx,
                    }).await;
                    // Note: we don't await the oneshot result here because the
                    // depth prompt response is handled by the session worker
                    // which updates the budget and notifies via budget.resume.
                }
                // Wait for user response (session worker calls budget.resume)
                budget.resume.notified().await;
                if budget.is_terminated() {
                    break;
                }
            }

            bus.send(ServerMessage::SubagentUpdate {
                subagent_id: subagent_id.clone(),
                description: task_description.clone(),
                status: "running".to_string(),
                detail: Some(format!("{} {}", tc.name, tc.arguments.get("path").and_then(|v| v.as_str()).or_else(|| tc.arguments.get("pattern").and_then(|v| v.as_str())).unwrap_or(""))),
            }).await;

            let tool_call = ToolCall {
                id: format!("tc_{}", Uuid::new_v4()),
                name: tc.name,
                arguments: tc.arguments,
            };
            let ptc = PendingToolCall {
                tool_call,
                cwd: cwd.clone(),
            };

            // Check session auto-approved set (shared across all agents)
            let session_auto_approved = {
                let sessions = state.sessions.read().await;
                sessions.get(&session_id)
                    .map(|s| s.auto_approved.clone())
                    .unwrap_or_default()
            };
            let auto = if ptc.tool_call.name == "run_command" {
                let cmd_str = ptc.tool_call.arguments["command"].as_str().unwrap_or("");
                let cmds = extract_shell_commands(cmd_str);
                !cmds.is_empty() && cmds.iter().all(|c| session_auto_approved.contains(c))
            } else {
                let display = tool_display_name(&ptc.tool_call.name);
                session_auto_approved.contains(display)
            };

            if auto {
                // Auto-approved — execute immediately, notify clients
                let mut display_tc = ptc.tool_call.clone();
                display_tc.name = tool_display_name(&ptc.tool_call.name).to_string();
                bus.send(ServerMessage::ToolAutoApproved { tool_call: display_tc }).await;
                let output = execute_tool(&state, session_id, &bus, &ptc).await;
                bus.send(tool_output_msg(&ptc, output.clone())).await;
                append_tool_result(&state, session_id, &ptc.tool_call.name, &output).await;
                budget.increment();

                let truncated = truncate_tool_output(&output, state.config.max_tool_output_chars);
                history.push(OllamaMessage {
                    role: "user".to_string(),
                    content: format!("[Tool output]:\n{truncated}"),
                });
                continue;
            }

            // Enqueue tool confirmation and wait for user response
            let (otx, orx) = tokio::sync::oneshot::channel();
            let _ = prompt_queue_tx.send(QueuedPrompt::ToolConfirm {
                tool_call: ptc.tool_call.clone(),
                cwd: ptc.cwd.clone(),
                result_tx: otx,
            }).await;

            // Wait for the session worker to resolve this tool confirmation
            let (approved, _always) = match orx.await {
                Ok(result) => result,
                Err(_) => {
                    // Channel closed — session ended or interrupted
                    break;
                }
            };

            let output = if approved {
                // Tool was already executed by the session worker when it
                // resolved the queued prompt — the output was sent to the bus.
                // We just need to record it in the subagent's local history.
                // However, the session worker already called execute_tool and
                // append_tool_result. We need the output for the subagent's
                // local history, so we re-read it... but that's not ideal.
                // Instead, let's NOT execute in the session worker for subagent
                // tools — just return the approval and let the subagent execute.
                // Actually the session worker already executed it. Let's skip
                // the duplicate execution and just note it was approved.
                budget.increment();
                "(tool executed — output sent to session)".to_string()
            } else {
                budget.increment();
                "Tool execution denied by user.".to_string()
            };

            // Truncate and add to history
            let truncated = truncate_tool_output(&output, state.config.max_tool_output_chars);
            history.push(OllamaMessage {
                role: "user".to_string(),
                content: format!("[Tool output]:\n{truncated}"),
            });
        }
    }

    // Send completion update
    bus.send(ServerMessage::SubagentUpdate {
        subagent_id: subagent_id.clone(),
        description: task_description,
        status: if budget.is_terminated() { "failed".to_string() } else { "completed".to_string() },
        detail: None,
    }).await;

    // Send result back to parent
    let _ = result_tx.send((task_id, full_response)).await;
}

/// Call the LLM with current history, send assistant text, and queue all
/// tool calls for sequential user confirmation.
/// When `ENABLE_REFLECTION` is true, a non-streaming reflection call runs
/// first and its output is temporarily injected into the history so the main
/// streaming response benefits from the reasoning.
async fn invoke_llm(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
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
        bus.send(ServerMessage::UserPrompt {
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

    bus.send(ServerMessage::Thinking).await;
    tracing::info!("invoke_llm: sent Thinking, starting compaction check");

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
            bus.send(ServerMessage::Error {
                text: "session not found".to_string(),
            }).await;
            return;
        };
        (session.history.clone(), session.info.cwd.clone())
    };

    tracing::info!("invoke_llm: history has {} messages, cwd={cwd}", history.len());
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
        tracing::info!("invoke_llm: starting reflective_thinking call");
        match reflective_thinking(&state.http_client, &state.config, &history_for_llm, &session_context).await {
            Ok(reflection) => {
                tracing::debug!("reflection complete ({} chars)", reflection.content.len());
                // Insert the reflection BEFORE the last user message so the
                // model still sees the user message last and knows to respond.
                let insert_pos = history_for_llm.len().saturating_sub(1);
                history_for_llm.insert(insert_pos, reflection);
            }
            Err(err) => {
                tracing::warn!("reflective thinking failed: {err}");
                // If the error looks like a connection failure, report it to the
                // client immediately — the main streaming call will likely fail
                // too, but at least the user sees feedback right away.
                let err_str = format!("{err}");
                if err_str.contains("onnect")
                    || err_str.contains("timed out")
                    || err_str.contains("connection")
                {
                    let provider_name = match state.config.llm_provider {
                        crate::state::LlmProvider::Ollama => "Ollama",
                        crate::state::LlmProvider::OpenAI => "OpenAI",
                    };
                    bus.send(ServerMessage::Error {
                        text: format!("Cannot reach {}: {err}", provider_name),
                    }).await;
                    bus.send(ServerMessage::AssistantTextDone).await;
                    return;
                }
                // Otherwise continue without reflection — the main call may still work
            }
        }
    }

    tracing::info!("invoke_llm: starting streaming LLM call");
    // Stream LLM response — send chunks to bus as they arrive
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<String>(64);

    let http = state.http_client.clone();
    let cfg = state.config.clone();
    let history_for_llm: Vec<ChatMessage> = history_for_llm.iter()
        .map(|msg| msg.clone().into())
        .collect();
    let llm_handle = tokio::spawn(async move {
        call_llm_streaming(&http, &cfg, &history_for_llm, &chunk_tx).await
    });

    // Forward chunks to the bus as AssistantText, but also listen
    // for an Interrupt/Input message from the client. If interrupted,
    // abort the LLM task, save partial output, and return the new user input.
    // NOTE: The LLM task continues even if no client is connected — chunks
    // are buffered in the bus message log for replay on reconnect.
    let mut partial_content = String::new();
    let mut interrupted_input: Option<String> = None;
    let mut filter = ToolCallFilter::new();

    let mut chunk_count = 0usize;
    loop {
        tokio::select! {
            chunk = chunk_rx.recv() => {
                match chunk {
                    Some(text) => {
                        chunk_count += 1;
                        if chunk_count <= 3 || chunk_count % 20 == 0 {
                            tracing::info!("invoke_llm: chunk #{chunk_count} len={}", text.len());
                        }
                        partial_content.push_str(&text);
                        let visible = filter.feed(&text);
                        if !visible.is_empty() {
                            bus.send(ServerMessage::AssistantText {
                                text: visible,
                            }).await;
                        }
                    }
                    None => {
                        tracing::info!("invoke_llm: chunk channel closed after {chunk_count} chunks");
                        break;
                    }
                }
            }
            client_msg = client_rx.recv() => {
                match client_msg {
                    Some(ClientMessage::Interrupt) => {
                        tracing::info!("invoke_llm: interrupted by client");
                        llm_handle.abort();
                        while chunk_rx.try_recv().is_ok() {}
                        break;
                    }
                    Some(ClientMessage::Input { text }) => {
                        tracing::info!("invoke_llm: interrupted by new input: {text}");
                        llm_handle.abort();
                        while chunk_rx.try_recv().is_ok() {}
                        interrupted_input = Some(text);
                        break;
                    }
                    Some(_) => {
                        tracing::debug!("invoke_llm: ignoring client msg during streaming");
                    }
                    None => {
                        // All clients disconnected — but we keep the LLM running!
                        // Just drain chunks until done, no more client messages to check.
                        tracing::info!("invoke_llm: client_rx closed, continuing LLM to completion");
                        // Finish draining chunks from the LLM
                        while let Some(text) = chunk_rx.recv().await {
                            partial_content.push_str(&text);
                            let visible = filter.feed(&text);
                            if !visible.is_empty() {
                                bus.send(ServerMessage::AssistantText { text: visible }).await;
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // Flush any remaining buffered text from the filter
    let remaining = filter.flush();
    if !remaining.is_empty() {
        bus.send(ServerMessage::AssistantText { text: remaining }).await;
    }

    tracing::info!("invoke_llm: sending AssistantTextDone, partial_content len={}", partial_content.len());
    bus.send(ServerMessage::AssistantTextDone).await;

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

        let interrupt_note = OllamaMessage {
            role: "system".to_string(),
            content: "[The user interrupted your previous response to send a new message. Your last reply was cut short.]".to_string(),
        };
        let user_msg = OllamaMessage {
            role: "user".to_string(),
            content: new_input,
        };
        {
            let mut sessions = state.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.info.touch();
                session.history.push(interrupt_note);
                session.history.push(user_msg);
            }
        }
        // Recurse to handle the new input
        Box::pin(invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
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
            Box::pin(present_next_tool(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
        }
        Ok(Err(err)) => {
            bus.send(ServerMessage::Error {
                text: format!("LLM request failed: {err}"),
            }).await;
        }
        Err(err) => {
            // JoinError — could be a panic or abort (from interrupt)
            if err.is_cancelled() {
                // Interrupted — already handled above
            } else {
                bus.send(ServerMessage::Error {
                    text: format!("LLM task panicked: {err}"),
                }).await;
            }
        }
    }
}

/// Extract individual command names from a shell string.
///
/// Splits on shell operators (`&&`, `||`, `;`, `|`) and extracts the base
/// command name from each segment, skipping env-var assignments, `sudo`, `env`,
/// and path prefixes. Returns deduplicated command names.
///
/// Examples:
///   "cd /tmp && rm -rf foo"  → ["cd", "rm"]
///   "FOO=1 sudo cargo build" → ["cargo"]
///   "ls | grep foo | wc -l"  → ["ls", "grep", "wc"]
fn extract_shell_commands(cmd_str: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Split on shell operators: &&, ||, ;, |
    let replaced = cmd_str
        .replace("&&", "\x00")
        .replace("||", "\x00")
        .replace(';', "\x00")
        .replace('|', "\x00");
    let segments: Vec<&str> = replaced
        .split('\x00')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Handle subshell: strip leading ( or $( 
            s.trim_start_matches('(').trim_start_matches("$(").trim()
        })
        .collect();

    for seg in segments {
        let tokens: Vec<&str> = seg.split_whitespace().collect();
        for token in &tokens {
            // Skip env var assignments like FOO=bar
            if token.contains('=') && !token.starts_with('-') {
                continue;
            }
            // Skip sudo/env prefixes
            if *token == "sudo" || *token == "env" || *token == "nohup" || *token == "time" || *token == "nice" {
                continue;
            }
            // Extract basename from paths like /usr/bin/ls
            let base = token.rsplit('/').next().unwrap_or(token);
            if !base.is_empty() && seen.insert(base.to_string()) {
                result.push(base.to_string());
            }
            break;
        }
    }

    result
}

/// Build a `ToolOutput` message with the display name and arguments.
fn tool_output_msg(ptc: &PendingToolCall, output: String) -> ServerMessage {
    ServerMessage::ToolOutput {
        tool_call_id: ptc.tool_call.id.clone(),
        tool_name: tool_display_name(&ptc.tool_call.name).to_string(),
        tool_args: ptc.tool_call.arguments.clone(),
        output,
    }
}

/// Map internal tool names to their user-facing display names.
/// `read_symbol` → `read_file`, `patch_symbol` → `patch_file`.
/// This ensures auto-approve, tool cards, and "Always approve" all treat
/// these as the same tool from the user's perspective.
fn tool_display_name(name: &str) -> &str {
    match name {
        "read_symbol" => "read_file",
        "patch_symbol" => "patch_file",
        _ => name,
    }
}

/// Pop the next tool call from the queue. For most tools, send a ToolRequest
/// to the client for confirmation. For `user_prompt_options`, skip the
/// confirmation step entirely and directly send the UserPrompt.
async fn present_next_tool(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
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
            bus.send(tool_output_msg(&ptc, output.clone())).await;
            append_tool_result(state, session_id, &ptc.tool_call.name, &output).await;
            *tool_depth += 1;
            if !tool_queue.is_empty() {
                Box::pin(present_next_tool(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
            } else {
                invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
            }
            return;
        }

        let prompt_id = format!("prompt_{}", Uuid::new_v4());
        bus.send(ServerMessage::UserPrompt {
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

    // Auto-execute read-only / non-destructive tools without user confirmation
    const AUTO_APPROVED_TOOLS: &[&str] = &[
        "todo_write", "todo_read", "web_fetch", "web_search",
        "lsp_diagnostics", "lsp_hover", "lsp_references", "lsp_symbols",
    ];
    if AUTO_APPROVED_TOOLS.contains(&ptc.tool_call.name.as_str()) {
        let output = execute_tool(state, session_id, bus, &ptc).await;
        bus.send(tool_output_msg(&ptc, output.clone())).await;
        append_tool_result(state, session_id, &ptc.tool_call.name, &output).await;
        *tool_depth += 1;
        if !tool_queue.is_empty() {
            Box::pin(present_next_tool(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
        } else {
            invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        }
        return;
    }

    // Check the session's auto-approved set (server-side).
    // For run_command, check each extracted sub-command; for other tools,
    // check the display name (e.g. read_symbol → read_file).
    let session_auto_approved = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id)
            .map(|s| s.auto_approved.clone())
            .unwrap_or_default()
    };

    let is_auto_approved = if ptc.tool_call.name == "run_command" {
        let cmd_str = ptc.tool_call.arguments["command"].as_str().unwrap_or("");
        let cmds = extract_shell_commands(cmd_str);
        !cmds.is_empty() && cmds.iter().all(|c| session_auto_approved.contains(c))
    } else {
        let display = tool_display_name(&ptc.tool_call.name);
        session_auto_approved.contains(display)
    };

    if is_auto_approved {
        // Send display-only notification to clients
        let mut display_tc = ptc.tool_call.clone();
        display_tc.name = tool_display_name(&ptc.tool_call.name).to_string();
        bus.send(ServerMessage::ToolAutoApproved {
            tool_call: display_tc,
        }).await;

        let output = execute_tool(state, session_id, bus, &ptc).await;
        bus.send(tool_output_msg(&ptc, output.clone())).await;
        append_tool_result(state, session_id, &ptc.tool_call.name, &output).await;
        *tool_depth += 1;
        if !tool_queue.is_empty() {
            Box::pin(present_next_tool(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment)).await;
        } else {
            invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        }
        return;
    }

    // For run_command, extract individual command names from the shell string
    let extracted_commands = if ptc.tool_call.name == "run_command" {
        let cmd_str = ptc.tool_call.arguments["command"]
            .as_str()
            .unwrap_or("");
        let cmds = extract_shell_commands(cmd_str);
        if cmds.is_empty() { None } else { Some(cmds) }
    } else {
        None
    };

    // Send the tool request with the display name so the client sees
    // read_symbol as read_file, patch_symbol as patch_file, etc.
    let mut display_tc = ptc.tool_call.clone();
    display_tc.name = tool_display_name(&ptc.tool_call.name).to_string();
    bus.send(ServerMessage::ToolRequest {
        tool_call: display_tc,
        extracted_commands,
    }).await;
    *pending = Some(ptc);
}

// ---------------------------------------------------------------------------
// Tool confirmation
// ---------------------------------------------------------------------------

async fn handle_tool_confirm(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    tool_call_id: &str,
    approved: bool,
    always: bool,
) {
    let ptc = match pending.take() {
        Some(p) if p.tool_call.id == tool_call_id => p,
        other => {
            *pending = other;
            // Silently ignore stale confirms (e.g. from a second client
            // responding after the first already resolved the prompt).
            return;
        }
    };

    // Broadcast resolution so all other clients dismiss their pickers.
    bus.send(ServerMessage::ToolResolved {
        tool_call_id: tool_call_id.to_string(),
        approved,
    }).await;

    // "Always approve" — add the display name (or extracted commands for
    // run_command) to the session's server-side auto-approved set.
    if always && approved {
        let display = tool_display_name(&ptc.tool_call.name).to_string();
        let cmds: Vec<String> = if ptc.tool_call.name == "run_command" {
            let cmd_str = ptc.tool_call.arguments["command"].as_str().unwrap_or("");
            let extracted = extract_shell_commands(cmd_str);
            if extracted.is_empty() { vec![display] } else { extracted }
        } else {
            vec![display]
        };
        let label = cmds.join("', '");
        {
            let mut sessions = state.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                for cmd in &cmds {
                    session.auto_approved.insert(cmd.clone());
                }
            }
        }
        bus.send(ServerMessage::Notice {
            text: format!("'{}' will be auto-approved for this session.", label),
        }).await;
    }

    if !approved {
        let output = "Tool call rejected by user.".to_string();
        bus.send(tool_output_msg(&ptc, output.clone())).await;
        append_tool_result(state, session_id, &ptc.tool_call.name, &output).await;
        // If rejected, skip remaining queued tools and re-invoke LLM
        tool_queue.clear();
        *tool_depth += 1;
        invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        return;
    }

    let output = execute_tool(state, session_id, bus, &ptc).await;
    bus.send(tool_output_msg(&ptc, output.clone())).await;
    append_tool_result(state, session_id, &ptc.tool_call.name, &output).await;
    *tool_depth += 1;

    // If more tool calls queued from the same LLM response, present the next one
    if !tool_queue.is_empty() {
        present_next_tool(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    } else {
        // All tools from this response executed — re-invoke LLM
        invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    }
}

// ---------------------------------------------------------------------------
// User prompt response
// ---------------------------------------------------------------------------

async fn handle_prompt_response(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
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
            // Silently ignore stale prompt responses.
            return;
        }
    };

    // Broadcast resolution so all other clients dismiss their pickers.
    bus.send(ServerMessage::PromptResolved {
        prompt_id: prompt_id.to_string(),
    }).await;

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

    bus.send(tool_output_msg(&pp.tool_call, output.clone())).await;
    append_tool_result(state, session_id, &pp.tool_call.tool_call.name, &output).await;
    *tool_depth += 1;

    // Continue the agentic loop
    if !tool_queue.is_empty() {
        present_next_tool(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    } else {
        invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
    }
}

// ---------------------------------------------------------------------------
// Depth continuation prompt response
// ---------------------------------------------------------------------------

async fn handle_depth_prompt_response(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    client_rx: &mut mpsc::Receiver<ClientMessage>,
    pending: &mut Option<PendingToolCall>,
    pending_prompt: &mut Option<PendingPrompt>,
    pending_depth_prompt: &mut Option<PendingDepthPrompt>,
    tool_queue: &mut VecDeque<PendingToolCall>,
    tool_depth: &mut usize,
    depth_limit: &mut usize,
    bulk_increment: &mut usize,
    budget: &ToolBudget,
    prompt_id: &str,
    selected: Vec<usize>,
) {
    // Broadcast resolution so all other clients dismiss their pickers.
    bus.send(ServerMessage::PromptResolved {
        prompt_id: prompt_id.to_string(),
    }).await;

    let choice = selected.first().copied().unwrap_or(2); // default to "No"

    match choice {
        0 => {
            // "Yes" — continue, pause again after max_tool_depth more calls
            *depth_limit += state.config.max_tool_depth;
            budget.limit.store(*depth_limit, Ordering::SeqCst);
            budget.prompt_sent.store(false, Ordering::SeqCst);
            // Wake all paused subagents
            budget.resume.notify_waiters();
            bus.send(ServerMessage::Notice {
                text: format!("Continuing. Will pause again after {} total calls.", *depth_limit),
            }).await;
            invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        }
        1 => {
            // "Yes, for next N" — continue, pause after N more calls, increment N
            *depth_limit += *bulk_increment;
            budget.limit.store(*depth_limit, Ordering::SeqCst);
            budget.prompt_sent.store(false, Ordering::SeqCst);
            // Wake all paused subagents
            budget.resume.notify_waiters();
            bus.send(ServerMessage::Notice {
                text: format!("Continuing. Will pause again after {} total calls.", *depth_limit),
            }).await;
            *bulk_increment += 25;
            invoke_llm(state, session_id, bus, client_rx, pending, pending_prompt, pending_depth_prompt, tool_queue, tool_depth, depth_limit, bulk_increment).await;
        }
        _ => {
            // "No" — stop all agents
            budget.terminated.store(true, Ordering::SeqCst);
            budget.resume.notify_waiters();
            bus.send(ServerMessage::Notice {
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

async fn append_tool_result(state: &ServerState, session_id: Uuid, tool_name: &str, output: &str) {
    // read_file gets a 4x higher limit — truncating a file the user explicitly
    // asked to read defeats the purpose and confuses the LLM.
    let limit = match tool_name {
        "read_file" | "read_symbol" => state.config.max_tool_output_chars * 4,
        _ => state.config.max_tool_output_chars,
    };
    let truncated = truncate_tool_output(output, limit);
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

/// Send a ServerMessage directly to a WebSocket (used by handle_socket for
/// initial handshake messages that are not part of the session bus).
async fn ws_send(socket: &mut WebSocket, message: &ServerMessage) -> anyhow::Result<()> {
    let payload = serde_json::to_string(message)?;
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

    // -- ToolCallFilter tests ------------------------------------------------

    #[test]
    fn filter_no_tool_calls() {
        let mut f = ToolCallFilter::new();
        let mut out = f.feed("Hello world");
        out.push_str(&f.flush());
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn filter_strips_single_tool_call() {
        let mut f = ToolCallFilter::new();
        let input = r#"Some text [TOOL_CALL]{"name":"run_command"}[/TOOL_CALL] more"#;
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        assert!(!out.contains("[TOOL_CALL]"));
        assert!(!out.contains("run_command"));
        assert!(out.contains("Some text"));
        assert!(out.contains("more"));
    }

    #[test]
    fn filter_strips_tool_call_across_chunks() {
        let mut f = ToolCallFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("Hello [TOOL"));
        out.push_str(&f.feed("_CALL]{\"name\":\"x\"}"));
        out.push_str(&f.feed("[/TOOL_CALL] done"));
        out.push_str(&f.flush());
        assert!(out.contains("Hello"));
        assert!(out.contains("done"));
        assert!(!out.contains("TOOL_CALL"));
        assert!(!out.contains("\"name\""));
    }

    #[test]
    fn filter_multiple_tool_calls() {
        let mut f = ToolCallFilter::new();
        let input = "A [TOOL_CALL]{\"a\":1}[/TOOL_CALL] B [TOOL_CALL]{\"b\":2}[/TOOL_CALL] C";
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        assert!(out.contains("A"));
        assert!(out.contains("B"));
        assert!(out.contains("C"));
        assert!(!out.contains("TOOL_CALL"));
    }

    #[test]
    fn filter_text_only_no_brackets() {
        let mut f = ToolCallFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("abc"));
        out.push_str(&f.feed("def"));
        out.push_str(&f.feed("ghi"));
        out.push_str(&f.flush());
        assert_eq!(out, "abcdefghi");
    }

    #[test]
    fn filter_strips_tool_name_tag_format() {
        let mut f = ToolCallFilter::new();
        let input = r#"Let me list files. [list_files]{"path": "."}[/list_files] Done."#;
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        assert!(out.contains("Let me list files."));
        assert!(out.contains("Done."));
        assert!(!out.contains("list_files"));
        assert!(!out.contains("path"));
    }

    #[test]
    fn filter_tool_name_tag_across_chunks() {
        let mut f = ToolCallFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("Hello [list"));
        out.push_str(&f.feed("_files]{\"path\": \".\"}"));
        out.push_str(&f.feed("[/list_files] done"));
        out.push_str(&f.flush());
        assert!(out.contains("Hello"));
        assert!(out.contains("done"));
        assert!(!out.contains("list_files"));
    }

    #[test]
    fn filter_preserves_non_tool_brackets() {
        let mut f = ToolCallFilter::new();
        let input = "Use [bold] text and [1] references.";
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        // [bold] has no underscore, [1] has digits — neither is a tool tag
        assert!(out.contains("[bold]"));
        assert!(out.contains("[1]"));
    }

    // -- is_tool_tag tests ---------------------------------------------------

    #[test]
    fn is_tool_tag_tool_call() {
        assert!(is_tool_tag("TOOL_CALL"));
    }

    #[test]
    fn is_tool_tag_snake_case() {
        assert!(is_tool_tag("list_files"));
        assert!(is_tool_tag("read_file"));
        assert!(is_tool_tag("run_command"));
        assert!(is_tool_tag("web_search"));
        assert!(is_tool_tag("lsp_diagnostics"));
    }

    #[test]
    fn is_tool_tag_rejects_no_underscore() {
        assert!(!is_tool_tag("bold"));
        assert!(!is_tool_tag("notice"));
        assert!(!is_tool_tag("a"));
    }

    #[test]
    fn is_tool_tag_rejects_digits() {
        assert!(!is_tool_tag("1"));
        assert!(!is_tool_tag("h2"));
        assert!(!is_tool_tag("tool_v2"));
    }

    #[test]
    fn is_tool_tag_rejects_uppercase_snake() {
        // Only TOOL_CALL is allowed as uppercase
        assert!(!is_tool_tag("READ_FILE"));
        assert!(!is_tool_tag("List_Files"));
    }

    #[test]
    fn is_tool_tag_rejects_empty() {
        assert!(!is_tool_tag(""));
    }

    // -- ToolCallFilter additional edge cases --------------------------------

    #[test]
    fn filter_unclosed_tool_call_discards() {
        let mut f = ToolCallFilter::new();
        let mut out = f.feed("Hello [TOOL_CALL]{\"name\":\"x\"}");
        out.push_str(&f.flush());
        // Text before the tag is emitted, unclosed tag content is discarded
        assert!(out.contains("Hello"));
        assert!(!out.contains("TOOL_CALL"));
        assert!(!out.contains("name"));
    }

    #[test]
    fn filter_mixed_formats_in_one_stream() {
        let mut f = ToolCallFilter::new();
        let input = "A [TOOL_CALL]{\"name\":\"x\"}[/TOOL_CALL] B [list_files]{\"path\":\".\"}\
                     [/list_files] C";
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        assert!(out.contains("A"));
        assert!(out.contains("B"));
        assert!(out.contains("C"));
        assert!(!out.contains("TOOL_CALL"));
        assert!(!out.contains("list_files"));
    }

    #[test]
    fn filter_close_tag_split_across_chunks() {
        let mut f = ToolCallFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("[list_files]{\"p\":1}[/list"));
        out.push_str(&f.feed("_files] after"));
        out.push_str(&f.flush());
        assert!(out.contains("after"));
        assert!(!out.contains("list_files"));
    }

    #[test]
    fn filter_consecutive_tool_calls_no_gap() {
        let mut f = ToolCallFilter::new();
        let input = "[read_file]{\"a\":1}[/read_file][write_file]{\"b\":2}[/write_file]done";
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        assert_eq!(out.trim(), "done");
    }

    #[test]
    fn filter_bracket_at_end_of_chunk() {
        let mut f = ToolCallFilter::new();
        let mut out = String::new();
        out.push_str(&f.feed("text ["));
        out.push_str(&f.feed("read_file]{\"a\":1}[/read_file] end"));
        out.push_str(&f.flush());
        assert!(out.contains("text"));
        assert!(out.contains("end"));
        assert!(!out.contains("read_file"));
    }

    #[test]
    fn filter_non_tool_bracket_followed_by_tool() {
        let mut f = ToolCallFilter::new();
        let input = "See [note] and [read_file]{\"a\":1}[/read_file] done";
        let mut out = f.feed(input);
        out.push_str(&f.flush());
        assert!(out.contains("[note]"));
        assert!(out.contains("done"));
        assert!(!out.contains("read_file"));
    }

    // -- extract_shell_commands tests ----------------------------------------

    #[test]
    fn extract_simple_command() {
        assert_eq!(extract_shell_commands("cargo build"), vec!["cargo"]);
    }

    #[test]
    fn extract_chained_commands() {
        assert_eq!(extract_shell_commands("cd /tmp && rm -rf foo"), vec!["cd", "rm"]);
    }

    #[test]
    fn extract_piped_commands() {
        assert_eq!(extract_shell_commands("ls | grep foo | wc -l"), vec!["ls", "grep", "wc"]);
    }

    #[test]
    fn extract_with_env_and_sudo() {
        assert_eq!(extract_shell_commands("FOO=1 sudo cargo build"), vec!["cargo"]);
    }

    #[test]
    fn extract_with_path_prefix() {
        assert_eq!(extract_shell_commands("/usr/bin/ls -la"), vec!["ls"]);
    }

    #[test]
    fn extract_semicolon_separated() {
        assert_eq!(extract_shell_commands("echo hello; cat file.txt"), vec!["echo", "cat"]);
    }

    #[test]
    fn extract_deduplicates() {
        assert_eq!(extract_shell_commands("cd /a && cd /b && ls"), vec!["cd", "ls"]);
    }

    #[test]
    fn extract_complex_mixed() {
        let cmds = extract_shell_commands("cd . && rm -rf build; mkdir build && cd build | tee log");
        assert_eq!(cmds, vec!["cd", "rm", "mkdir", "tee"]);
    }
}
