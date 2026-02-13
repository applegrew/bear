mod menu;
mod term;

use anyhow::Context;
use bear_core::{
    ClientMessage, CreateSessionRequest, CreateSessionResponse, SessionListResponse, ServerMessage,
    ToolCall, DEFAULT_SERVER_URL,
};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use menu::{interactive_menu, MenuItem, MenuMode, MenuResult};
use reqwest::Url;
use std::collections::HashSet;
use std::sync::mpsc as std_mpsc;
use term::{RenderCmd, TermEvent, ToolConfirmChoice};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "bear", about = "Bear client for persistent sessions")]
struct Cli {
    #[arg(long)]
    server_url: Option<String>,
    #[arg(long)]
    session: Option<Uuid>,
    #[arg(long)]
    new_session: bool,
}

#[derive(Debug, PartialEq)]
enum SessionResult {
    EndSession,
    Quit,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let server_url = cli
        .server_url
        .or_else(|| std::env::var("BEAR_SERVER_URL").ok())
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
    let base_url = Url::parse(&server_url).context("invalid server URL")?;

    let http_client = reqwest::Client::new();

    // First session: use CLI args
    let mut session_id = if let Some(id) = cli.session {
        id
    } else {
        resolve_session(&http_client, &base_url, cli.new_session).await?
    };

    loop {
        let result = connect_session(&base_url, session_id).await?;
        if result == SessionResult::Quit {
            break;
        }
        // EndSession: go back to session selection
        session_id = resolve_session(&http_client, &base_url, false).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Session selection (runs before raw mode)
// ---------------------------------------------------------------------------

async fn resolve_session(
    http_client: &reqwest::Client,
    base_url: &Url,
    force_new: bool,
) -> anyhow::Result<Uuid> {
    let sessions_url = base_url.join("/sessions")?;
    let response = http_client
        .get(sessions_url)
        .send()
        .await?
        .error_for_status()?;
    let list: SessionListResponse = response.json().await?;

    if list.sessions.is_empty() || force_new {
        return create_session(http_client, base_url).await;
    }

    // Build menu items: existing sessions + "New session" option
    let mut items: Vec<MenuItem> = list
        .sessions
        .iter()
        .map(|s| MenuItem {
            label: s.name.clone().unwrap_or_else(|| format!("{}", s.id)),
            description: format!("{} | created {}", s.cwd, s.created_at.format("%Y-%m-%d %H:%M")),
        })
        .collect();
    items.push(MenuItem {
        label: "+ New session".to_string(),
        description: String::new(),
    });

    // Print some blank lines so the menu redraw doesn't overshoot
    let total_lines = items.len() + 2;
    for _ in 0..total_lines {
        println!();
    }

    match interactive_menu("Select a session:", &items, MenuMode::Single) {
        MenuResult::Single(idx) if idx < list.sessions.len() => {
            Ok(list.sessions[idx].id)
        }
        MenuResult::Single(_) => {
            // "New session" was selected
            create_session(http_client, base_url).await
        }
        MenuResult::Cancelled => {
            // Default: create new
            println!("Cancelled; creating new session.");
            create_session(http_client, base_url).await
        }
        _ => create_session(http_client, base_url).await,
    }
}

async fn create_session(http_client: &reqwest::Client, base_url: &Url) -> anyhow::Result<Uuid> {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(|s| s.to_string()));

    let sessions_url = base_url.join("/sessions")?;
    let response = http_client
        .post(sessions_url)
        .json(&CreateSessionRequest { cwd })
        .send()
        .await?
        .error_for_status()?;
    let created: CreateSessionResponse = response.json().await?;
    Ok(created.session.id)
}

// ---------------------------------------------------------------------------
// Connected session loop
// ---------------------------------------------------------------------------

enum LoopEvent {
    FromServer(ServerMessage),
    FromTerm(TermEvent),
}

async fn connect_session(base_url: &Url, session_id: Uuid) -> anyhow::Result<SessionResult> {
    let ws_url = to_ws_url(base_url, session_id)?;
    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Channel: server messages forwarded into the main loop
    let (srv_tx, mut srv_rx) = mpsc::channel::<ServerMessage>(64);
    tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            if let Message::Text(text) = msg {
                if let Ok(m) = serde_json::from_str::<ServerMessage>(&text) {
                    if srv_tx.send(m).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Channel: terminal events (user lines, quit)
    let (term_event_tx, mut term_event_rx) = mpsc::channel::<TermEvent>(32);

    // Channel: render commands to the terminal thread (std::sync so the
    // terminal thread can use try_recv without async)
    let (render_tx, render_rx) = std_mpsc::channel::<RenderCmd>();

    // Spawn the single terminal owner thread
    let term_handle = term::spawn_terminal_thread(render_rx, term_event_tx);

    let mut auto_approved: HashSet<String> = HashSet::new();
    let mut last_tool: (String, serde_json::Value) = (String::new(), serde_json::Value::Null);

    loop {
        let event = tokio::select! {
            Some(msg) = srv_rx.recv() => LoopEvent::FromServer(msg),
            Some(te) = term_event_rx.recv() => LoopEvent::FromTerm(te),
            else => break,
        };

        match event {
            LoopEvent::FromServer(msg) => {
                if let Some(auto_confirm) = dispatch_server_msg(
                    &msg, &render_tx, &auto_approved, &mut last_tool,
                ) {
                    // Auto-approved tool call — send confirmation immediately
                    let payload = serde_json::to_string(&ClientMessage::ToolConfirm {
                        tool_call_id: auto_confirm,
                        approved: true,
                    })?;
                    ws_write.send(Message::Text(payload)).await?;
                }
            }
            LoopEvent::FromTerm(TermEvent::ToolConfirmResult { tool_call_id, base_command, choice }) => {
                let approved = choice != ToolConfirmChoice::Deny;
                if choice == ToolConfirmChoice::Always {
                    auto_approved.insert(base_command.clone());
                    let _ = render_tx.send(RenderCmd::Notice(
                        format!("'{}' will be auto-approved for this session.", base_command),
                    ));
                }
                let payload = serde_json::to_string(&ClientMessage::ToolConfirm {
                    tool_call_id,
                    approved,
                })?;
                ws_write.send(Message::Text(payload)).await?;
            }
            LoopEvent::FromTerm(TermEvent::UserLine(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                // Slash commands
                if line == "/ps" {
                    let payload = serde_json::to_string(&ClientMessage::ProcessList)?;
                    ws_write.send(Message::Text(payload)).await?;
                } else if let Some(rest) = line.strip_prefix("/kill ") {
                    match rest.trim().parse::<u32>() {
                        Ok(pid) => {
                            let payload = serde_json::to_string(
                                &ClientMessage::ProcessKill { pid },
                            )?;
                            ws_write.send(Message::Text(payload)).await?;
                        }
                        Err(_) => {
                            let _ = render_tx.send(RenderCmd::Error(
                                "Usage: /kill <pid>".into(),
                            ));
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("/send ") {
                    if let Some((pid_str, text)) = rest.split_once(' ') {
                        if let Ok(pid) = pid_str.trim().parse::<u32>() {
                            let payload = serde_json::to_string(
                                &ClientMessage::ProcessInput {
                                    pid,
                                    text: text.to_string(),
                                },
                            )?;
                            ws_write.send(Message::Text(payload)).await?;
                        } else {
                            let _ = render_tx.send(RenderCmd::Error(
                                "Usage: /send <pid> <text>".into(),
                            ));
                        }
                    } else {
                        let _ = render_tx.send(RenderCmd::Error(
                            "Usage: /send <pid> <text>".into(),
                        ));
                    }
                } else if line == "/end" {
                    // Tell the server to delete this session
                    let payload = serde_json::to_string(&ClientMessage::SessionEnd)?;
                    ws_write.send(Message::Text(payload)).await?;
                    let _ = render_tx.send(RenderCmd::Notice(
                        "Session ended. Returning to session selection...".into(),
                    ));
                    let _ = render_tx.send(RenderCmd::Quit);
                    drop(render_tx);
                    let _ = term_handle.join();
                    return Ok(SessionResult::EndSession);
                } else if line == "/exit" {
                    let _ = render_tx.send(RenderCmd::Notice(
                        "Disconnecting. Session preserved. Returning to session selection...".into(),
                    ));
                    let _ = render_tx.send(RenderCmd::Quit);
                    drop(render_tx);
                    let _ = term_handle.join();
                    return Ok(SessionResult::EndSession);
                } else if line == "/allowed" {
                    if auto_approved.is_empty() {
                        let _ = render_tx.send(RenderCmd::Notice(
                            "No auto-approved commands.".into(),
                        ));
                    } else {
                        let mut cmds: Vec<&str> = auto_approved.iter().map(|s| s.as_str()).collect();
                        cmds.sort();
                        let _ = render_tx.send(RenderCmd::Notice(
                            format!("Auto-approved commands: {}", cmds.join(", ")),
                        ));
                    }
                } else if let Some(rest) = line.strip_prefix("/session ") {
                    if let Some(name) = rest.strip_prefix("name ") {
                        let name = name.trim();
                        if name.is_empty() {
                            let _ = render_tx.send(RenderCmd::Error(
                                "Usage: /session name <session name>".into(),
                            ));
                        } else {
                            let payload = serde_json::to_string(
                                &ClientMessage::SessionRename { name: name.to_string() },
                            )?;
                            ws_write.send(Message::Text(payload)).await?;
                        }
                    } else {
                        let _ = render_tx.send(RenderCmd::Error(
                            "Usage: /session name <session name>".into(),
                        ));
                    }
                } else if line == "/help" {
                    let help = [
                        "Commands:",
                        "  /ps              List background processes",
                        "  /kill <pid>      Kill a background process",
                        "  /send <pid> <text>  Send stdin to a process",
                        "  /session name <n>  Name the current session",
                        "  /allowed         Show auto-approved commands",
                        "  /exit            Disconnect, keep session alive",
                        "  /end             End current session, pick another",
                        "  /help            Show this help",
                        "  Ctrl+D           Quit",
                        "",
                        "Tool confirmations:  (interactive picker)",
                        "  Approve          Allow this tool call",
                        "  Deny             Reject this tool call",
                        "  Always approve   Auto-approve this command for the session",
                        "",
                        "Agent tools:",
                        "  run_command      Execute shell commands",
                        "  read_file        Read file contents",
                        "  write_file       Create/overwrite files",
                        "  edit_file        Surgical find-and-replace",
                        "  patch_file       Apply unified diffs",
                        "  list_files       Directory listing with glob",
                        "  search_text      Regex search across files",
                        "  undo             Revert file changes",
                        "  user_prompt_options  Present choices to user",
                    ]
                    .join("\n");
                    let _ = render_tx.send(RenderCmd::Notice(help));
                } else {
                    // Regular chat input -> send to server/LLM
                    let payload = serde_json::to_string(
                        &ClientMessage::Input { text: line },
                    )?;
                    ws_write.send(Message::Text(payload)).await?;
                }
            }
            LoopEvent::FromTerm(TermEvent::Interrupt) => {
                // Ctrl+C — no-op (tool confirmation is handled by picker)
            }
            LoopEvent::FromTerm(TermEvent::UserPromptResult { prompt_id, selected }) => {
                let payload = serde_json::to_string(
                    &ClientMessage::UserPromptResponse { prompt_id, selected },
                )?;
                ws_write.send(Message::Text(payload)).await?;
            }
            LoopEvent::FromTerm(TermEvent::Quit) => {
                let _ = render_tx.send(RenderCmd::Quit);
                drop(render_tx);
                let _ = term_handle.join();
                return Ok(SessionResult::Quit);
            }
        }
    }

    Ok(SessionResult::Quit)
}

fn extract_base_command(tool_call: &ToolCall) -> String {
    match tool_call.name.as_str() {
        "run_command" => {
            if let Some(cmd) = tool_call.arguments.get("command").and_then(|v| v.as_str()) {
                // Extract the first token of the shell command.
                // Handle common patterns: env vars, sudo, path prefixes.
                let tokens: Vec<&str> = cmd.split_whitespace().collect();
                for token in &tokens {
                    // Skip env var assignments like FOO=bar
                    if token.contains('=') && !token.starts_with('-') {
                        continue;
                    }
                    // Skip sudo/env
                    if *token == "sudo" || *token == "env" {
                        continue;
                    }
                    // Extract basename from paths like /usr/bin/ls
                    let base = token.rsplit('/').next().unwrap_or(token);
                    return base.to_string();
                }
                cmd.to_string()
            } else {
                "run_command".to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Returns Some(tool_call_id) if the tool call was auto-approved and should be
/// confirmed immediately without user input.
fn dispatch_server_msg(
    msg: &ServerMessage,
    render_tx: &std_mpsc::Sender<RenderCmd>,
    auto_approved: &HashSet<String>,
    last_tool: &mut (String, serde_json::Value),
) -> Option<String> {
    match msg {
        ServerMessage::SessionInfo { session } => {
            let _ = std::env::set_current_dir(&session.cwd);
            let display_name = session.name.clone()
                .unwrap_or_else(|| session.id.to_string());
            let _ = render_tx.send(RenderCmd::SessionInfo(
                display_name,
                session.cwd.clone(),
            ));
        }
        ServerMessage::AssistantText { text } => {
            let _ = render_tx.send(RenderCmd::AssistantChunk(text.clone()));
        }
        ServerMessage::ToolRequest { tool_call } => {
            *last_tool = (tool_call.name.clone(), tool_call.arguments.clone());
            let base_cmd = extract_base_command(tool_call);
            let args_str = serde_json::to_string_pretty(&tool_call.arguments)
                .unwrap_or_else(|_| tool_call.arguments.to_string());

            if auto_approved.contains(&base_cmd) {
                // Auto-approved: show brief notice and return the ID for immediate confirm
                let _ = render_tx.send(RenderCmd::Notice(
                    format!("  [auto-approved] {} {}", tool_call.name, args_str.lines().next().unwrap_or("")),
                ));
                return Some(tool_call.id.clone());
            }

            let _ = render_tx.send(RenderCmd::ToolRequest {
                tool_call_id: tool_call.id.clone(),
                base_command: base_cmd,
                name: tool_call.name.clone(),
                args: args_str,
            });
        }
        ServerMessage::ToolOutput { output, .. } => {
            let _ = render_tx.send(RenderCmd::ToolOutput {
                tool_name: last_tool.0.clone(),
                tool_args: last_tool.1.clone(),
                output: output.clone(),
            });
        }
        ServerMessage::ProcessStarted { info } => {
            let _ = render_tx.send(RenderCmd::ProcessEvent(
                format!("Started pid={} cmd={}", info.pid, info.command),
            ));
        }
        ServerMessage::ProcessOutput { pid, text } => {
            let _ = render_tx.send(RenderCmd::ProcessEvent(
                format!("[{}] {}", pid, text),
            ));
        }
        ServerMessage::ProcessExited { pid, code } => {
            let code_str = code.map(|c| c.to_string()).unwrap_or("unknown".into());
            let _ = render_tx.send(RenderCmd::ProcessEvent(
                format!("Process {} exited (code {})", pid, code_str),
            ));
        }
        ServerMessage::ProcessListResult { processes } => {
            if processes.is_empty() {
                let _ = render_tx.send(RenderCmd::Notice(
                    "No background processes.".into(),
                ));
            } else {
                let mut lines = vec!["Background processes:".to_string()];
                for p in processes {
                    let status = if p.running { "running" } else { "exited" };
                    lines.push(format!("  pid={} [{}] {}", p.pid, status, p.command));
                }
                let _ = render_tx.send(RenderCmd::Notice(lines.join("\n")));
            }
        }
        ServerMessage::UserPrompt { prompt_id, question, options, multi } => {
            let _ = render_tx.send(RenderCmd::UserPrompt {
                prompt_id: prompt_id.clone(),
                question: question.clone(),
                options: options.clone(),
                multi: *multi,
            });
        }
        ServerMessage::SessionRenamed { name } => {
            let _ = render_tx.send(RenderCmd::Notice(
                format!("Session renamed to: {name}"),
            ));
        }
        ServerMessage::Notice { text } => {
            let _ = render_tx.send(RenderCmd::Notice(text.clone()));
        }
        ServerMessage::Error { text } => {
            let _ = render_tx.send(RenderCmd::Error(text.clone()));
        }
        ServerMessage::AssistantTextDone => {
            let _ = render_tx.send(RenderCmd::AssistantDone);
        }
        ServerMessage::Thinking => {
            let _ = render_tx.send(RenderCmd::Thinking);
        }
        ServerMessage::Pong => {}
    }
    None
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

fn to_ws_url(base_url: &Url, session_id: Uuid) -> anyhow::Result<Url> {
    let mut ws_url = base_url.clone();
    let scheme = match ws_url.scheme() {
        "https" => "wss",
        "http" => "ws",
        "wss" => "wss",
        "ws" => "ws",
        _ => "ws",
    };
    ws_url
        .set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid URL scheme"))?;
    ws_url.set_path(&format!("/ws/{session_id}"));
    ws_url.set_query(None);
    Ok(ws_url)
}
