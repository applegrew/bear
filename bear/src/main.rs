mod menu;
mod term;

use anyhow::Context;
use bear_core::{
    ClientMessage, CreateSessionRequest, CreateSessionResponse, SessionListResponse, ServerMessage,
    DEFAULT_SERVER_URL,
};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use menu::{interactive_menu, MenuItem, MenuMode, MenuResult};
use reqwest::Url;
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

    install_signal_handlers()?;

    let http_client = reqwest::Client::new();

    // First session: use CLI args
    let mut session_id = if let Some(id) = cli.session {
        id
    } else {
        match resolve_session(&http_client, &base_url, cli.new_session).await? {
            Some(id) => id,
            None => return Ok(()), // user cancelled — exit
        }
    };

    loop {
        let result = connect_session(&base_url, session_id).await?;
        if result == SessionResult::Quit {
            break;
        }
        // EndSession: go back to session selection
        session_id = match resolve_session(&http_client, &base_url, false).await? {
            Some(id) => id,
            None => break, // user cancelled — exit
        };
    }

    Ok(())
}

fn install_signal_handlers() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sighup = signal(SignalKind::hangup())?;

        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => {},
                _ = sigint.recv() => {},
                _ = sighup.recv() => {},
            }
            term::cleanup_terminal();
            std::process::exit(1);
        });
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
) -> anyhow::Result<Option<Uuid>> {
    let sessions_url = base_url.join("/sessions")?;
    let response = http_client
        .get(sessions_url)
        .send()
        .await?
        .error_for_status()?;
    let list: SessionListResponse = response.json().await?;

    if list.sessions.is_empty() || force_new {
        return create_session(http_client, base_url).await.map(Some);
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

    match interactive_menu("Select a session:", &items, MenuMode::Single) {
        MenuResult::Single(idx) if idx < list.sessions.len() => {
            Ok(Some(list.sessions[idx].id))
        }
        MenuResult::Single(_) => {
            // "New session" was selected
            create_session(http_client, base_url).await.map(Some)
        }
        MenuResult::Cancelled => {
            Ok(None)
        }
        _ => create_session(http_client, base_url).await.map(Some),
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

    let mut last_tool: (String, serde_json::Value) = (String::new(), serde_json::Value::Null);
    let mut slash_commands: Vec<(String, String)> = Vec::new();

    loop {
        let event = tokio::select! {
            Some(msg) = srv_rx.recv() => LoopEvent::FromServer(msg),
            Some(te) = term_event_rx.recv() => LoopEvent::FromTerm(te),
            else => break,
        };

        match event {
            LoopEvent::FromServer(msg) => {
                dispatch_server_msg(
                    &msg, &render_tx, &mut last_tool, &mut slash_commands,
                );
            }
            LoopEvent::FromTerm(TermEvent::ToolConfirmResult { tool_call_id, choice, .. }) => {
                let approved = choice != ToolConfirmChoice::Deny;
                let always = choice == ToolConfirmChoice::Always;
                let payload = serde_json::to_string(&ClientMessage::ToolConfirm {
                    tool_call_id,
                    approved,
                    always,
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
                    } else if let Some(path) = rest.strip_prefix("workdir ") {
                        let path = path.trim();
                        if path.is_empty() {
                            let _ = render_tx.send(RenderCmd::Error(
                                "Usage: /session workdir <path>".into(),
                            ));
                        } else {
                            let payload = serde_json::to_string(
                                &ClientMessage::SessionWorkdir { path: path.to_string() },
                            )?;
                            ws_write.send(Message::Text(payload)).await?;
                        }
                    } else {
                        let _ = render_tx.send(RenderCmd::Error(
                            "Usage: /session name <session name> OR /session workdir <path>".into(),
                        ));
                    }
                } else if line == "/help" {
                    let command_lines = if slash_commands.is_empty() {
                        vec!["  (commands not loaded yet)".to_string()]
                    } else {
                        slash_commands.iter()
                            .map(|(cmd, desc)| format!("  {cmd:<20} {desc}"))
                            .collect()
                    };
                    let mut help_lines = vec!["Commands:".to_string()];
                    help_lines.extend(command_lines);
                    help_lines.extend([
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
                    ].iter().map(|s| s.to_string()));
                    let help = help_lines.join("\n");
                    let _ = render_tx.send(RenderCmd::Notice(help));
                } else {
                    // Regular chat input -> send to server/LLM
                    let payload = serde_json::to_string(
                        &ClientMessage::Input { text: line },
                    )?;
                    ws_write.send(Message::Text(payload)).await?;
                }
            }
            LoopEvent::FromTerm(TermEvent::UserPromptResult { prompt_id, selected }) => {
                let payload = serde_json::to_string(
                    &ClientMessage::UserPromptResponse { prompt_id, selected },
                )?;
                ws_write.send(Message::Text(payload)).await?;
            }
            LoopEvent::FromTerm(TermEvent::TaskPlanResult { plan_id, approved }) => {
                let payload = serde_json::to_string(
                    &ClientMessage::TaskPlanResponse { plan_id, approved },
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

fn dispatch_server_msg(
    msg: &ServerMessage,
    render_tx: &std_mpsc::Sender<RenderCmd>,
    last_tool: &mut (String, serde_json::Value),
    slash_commands: &mut Vec<(String, String)>,
) {
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
        ServerMessage::SlashCommands { commands } => {
            let list: Vec<(String, String)> = commands.iter()
                .map(|cmd| (cmd.cmd.clone(), cmd.desc.clone()))
                .collect();
            *slash_commands = list.clone();
            let _ = render_tx.send(RenderCmd::SlashCommands(list));
        }
        ServerMessage::AssistantText { text } => {
            let _ = render_tx.send(RenderCmd::AssistantChunk(text.clone()));
        }
        ServerMessage::ToolRequest { tool_call, .. } => {
            *last_tool = (tool_call.name.clone(), tool_call.arguments.clone());
            let _ = render_tx.send(RenderCmd::ToolRequest {
                tool_call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                args: serde_json::to_string_pretty(&tool_call.arguments)
                    .unwrap_or_else(|_| tool_call.arguments.to_string()),
            });
        }
        ServerMessage::ToolAutoApproved { tool_call } => {
            *last_tool = (tool_call.name.clone(), tool_call.arguments.clone());
            let desc = term::format_tool_description(&tool_call.name, &tool_call.arguments);
            let mut card = format!("┌─ ⚡ {} ─ (auto-approved)\n", tool_call.name);
            for line in &desc {
                card.push_str(&format!("│  {line}\n"));
            }
            card.push_str("└─");
            let _ = render_tx.send(RenderCmd::Notice(card));
        }
        ServerMessage::ToolOutput { tool_name, tool_args, output, .. } => {
            *last_tool = (tool_name.clone(), tool_args.clone());
            let _ = render_tx.send(RenderCmd::ToolOutput {
                tool_name: tool_name.clone(),
                tool_args: tool_args.clone(),
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
            let _ = render_tx.send(RenderCmd::SessionRenamed(name.clone()));
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
        ServerMessage::ClientState { input_history } => {
            let _ = render_tx.send(RenderCmd::ClientState {
                input_history: input_history.clone(),
            });
        }
        ServerMessage::TaskPlan { plan_id, tasks } => {
            let task_tuples: Vec<(String, String, bool)> = tasks.iter()
                .map(|t| (t.id.clone(), t.description.clone(), t.needs_write))
                .collect();
            let _ = render_tx.send(RenderCmd::TaskPlan {
                plan_id: plan_id.clone(),
                tasks: task_tuples,
            });
        }
        ServerMessage::TaskProgress { plan_id, task_id, status, detail } => {
            let _ = render_tx.send(RenderCmd::TaskProgress {
                plan_id: plan_id.clone(),
                task_id: task_id.clone(),
                status: status.clone(),
                detail: detail.clone(),
            });
        }
        ServerMessage::SubagentUpdate { subagent_id, description, status, detail } => {
            let _ = render_tx.send(RenderCmd::SubagentUpdate {
                subagent_id: subagent_id.clone(),
                description: description.clone(),
                status: status.clone(),
                detail: detail.clone(),
            });
        }
        ServerMessage::ToolResolved { tool_call_id, approved } => {
            let _ = render_tx.send(RenderCmd::ToolResolved {
                tool_call_id: tool_call_id.clone(),
                approved: *approved,
            });
        }
        ServerMessage::PromptResolved { prompt_id } => {
            let _ = render_tx.send(RenderCmd::PromptResolved {
                prompt_id: prompt_id.clone(),
            });
        }
        ServerMessage::Pong => {}
    }
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
