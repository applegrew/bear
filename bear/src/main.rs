mod menu;
mod server;
mod setup;
mod term;

use anyhow::Context;
use bear_core::{
    ClientMessage, CreateSessionRequest, CreateSessionResponse, ServerMessage, SessionListResponse,
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

    /// Stop the running bear-server (does not launch a client session)
    #[arg(long)]
    stop: bool,
    /// Restart the bear-server (does not launch a client session)
    #[arg(long)]
    restart: bool,
    /// Persistently disable relay polling
    #[arg(long)]
    disable_relay: bool,
    /// Re-enable relay polling
    #[arg(long)]
    enable_relay: bool,
    /// Pair with a relay server using an invite code
    #[arg(long)]
    relay_pair: Option<String>,
    /// Revoke the current relay pairing
    #[arg(long)]
    relay_revoke: bool,
    /// Re-run the LLM setup wizard (reconfigure provider, model, API keys)
    #[arg(long)]
    setup: bool,
}

#[derive(Debug, PartialEq)]
enum SessionResult {
    EndSession,
    Quit,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // --- Signal flags: handle and exit early (no client session) ---

    if cli.stop {
        return server::stop_server().await;
    }

    if cli.restart {
        return server::restart_server().await;
    }

    if cli.disable_relay {
        let mut cfg = bear_core::ConfigFile::load();
        cfg.relay_disabled = Some(true);
        cfg.save().context("failed to save config")?;
        eprintln!("  Relay disabled.");
        server::prompt_restart_if_running().await?;
        return Ok(());
    }

    if cli.enable_relay {
        let mut cfg = bear_core::ConfigFile::load();
        cfg.relay_disabled = None;
        cfg.save().context("failed to save config")?;
        eprintln!("  Relay enabled.");
        server::prompt_restart_if_running().await?;
        return Ok(());
    }

    if cli.setup {
        setup::rerun_setup()?;
        server::prompt_restart_if_running().await?;
        return Ok(());
    }

    // Setup wizard (first-time only) + auto-launch server
    setup::ensure_config()?;
    server::ensure_server_running().await?;

    let server_url = cli
        .server_url
        .or_else(|| std::env::var("BEAR_SERVER_URL").ok())
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
    let base_url = Url::parse(&server_url).context("invalid server URL")?;

    // --- Relay pair/revoke: talk to bear-server, then exit ---

    if let Some(invite_code) = cli.relay_pair {
        let relay_url = std::env::var("BEAR_RELAY_URL")
            .unwrap_or_else(|_| "https://bear.applegrew.com/relay".to_string());
        let http = reqwest::Client::new();
        let res = http
            .post(format!("{}/relay/pair", server_url))
            .json(&serde_json::json!({ "relay_url": relay_url, "invite_code": invite_code }))
            .send()
            .await
            .context("failed to reach bear-server")?;
        let body: serde_json::Value = res
            .json()
            .await
            .context("invalid response from bear-server")?;
        if body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            let room_id = body["room_id"].as_str().unwrap_or("unknown");
            eprintln!("  Paired with relay successfully.");
            eprintln!("  Room ID: {room_id}");
            eprintln!("  Relay URL: {relay_url}");
        } else {
            let err = body["error"].as_str().unwrap_or("unknown error");
            eprintln!("  Pairing failed: {err}");
        }
        return Ok(());
    }

    if cli.relay_revoke {
        let http = reqwest::Client::new();
        let res = http
            .post(format!("{}/relay/revoke", server_url))
            .send()
            .await
            .context("failed to reach bear-server")?;
        let body: serde_json::Value = res
            .json()
            .await
            .context("invalid response from bear-server")?;
        if body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            eprintln!("  Relay pairing revoked.");
        } else {
            let err = body["error"].as_str().unwrap_or("unknown error");
            eprintln!("  Revoke failed: {err}");
        }
        return Ok(());
    }

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
        let result = match connect_session(&base_url, session_id).await {
            Ok(r) => r,
            Err(err) => {
                eprintln!("\n  Connection failed: {err:#}\n");
                // Return to session selection instead of crashing
                SessionResult::EndSession
            }
        };
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
            description: format!(
                "{} | created {}",
                s.cwd,
                s.created_at.format("%Y-%m-%d %H:%M")
            ),
        })
        .collect();
    items.push(MenuItem {
        label: "+ New session".to_string(),
        description: String::new(),
    });

    match interactive_menu("Select a session:", &items, MenuMode::Single) {
        MenuResult::Single(idx) if idx < list.sessions.len() => Ok(Some(list.sessions[idx].id)),
        MenuResult::Single(_) => {
            // "New session" was selected
            create_session(http_client, base_url).await.map(Some)
        }
        MenuResult::Cancelled => Ok(None),
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
    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect to {ws_url}"))?;
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
    let mut disconnected = false;

    // Helper macro: send a WS message, or break out of the main loop on error.
    macro_rules! ws_send {
        ($payload:expr) => {
            if ws_write.send(Message::Text($payload)).await.is_err() {
                disconnected = true;
                break;
            }
        };
    }

    loop {
        let event = tokio::select! {
            msg = srv_rx.recv() => match msg {
                Some(m) => LoopEvent::FromServer(m),
                None => { disconnected = true; break; }
            },
            te = term_event_rx.recv() => match te {
                Some(t) => LoopEvent::FromTerm(t),
                None => break,
            },
        };

        match event {
            LoopEvent::FromServer(msg) => {
                dispatch_server_msg(&msg, &render_tx, &mut last_tool, &mut slash_commands);
            }
            LoopEvent::FromTerm(TermEvent::ToolConfirmResult {
                tool_call_id,
                choice,
                ..
            }) => {
                let approved = choice != ToolConfirmChoice::Deny;
                let always = choice == ToolConfirmChoice::Always;
                let payload = serde_json::to_string(&ClientMessage::ToolConfirm {
                    tool_call_id,
                    approved,
                    always,
                })?;
                ws_send!(payload);
            }
            LoopEvent::FromTerm(TermEvent::UserLine(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                // Slash commands
                if line == "/ps" {
                    let payload = serde_json::to_string(&ClientMessage::ProcessList)?;
                    ws_send!(payload);
                } else if let Some(rest) = line.strip_prefix("/kill ") {
                    match rest.trim().parse::<u32>() {
                        Ok(pid) => {
                            let payload =
                                serde_json::to_string(&ClientMessage::ProcessKill { pid })?;
                            ws_send!(payload);
                        }
                        Err(_) => {
                            let _ = render_tx.send(RenderCmd::Error("Usage: /kill <pid>".into()));
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("/send ") {
                    if let Some((pid_str, text)) = rest.split_once(' ') {
                        if let Ok(pid) = pid_str.trim().parse::<u32>() {
                            let payload = serde_json::to_string(&ClientMessage::ProcessInput {
                                pid,
                                text: text.to_string(),
                            })?;
                            ws_send!(payload);
                        } else {
                            let _ = render_tx
                                .send(RenderCmd::Error("Usage: /send <pid> <text>".into()));
                        }
                    } else {
                        let _ =
                            render_tx.send(RenderCmd::Error("Usage: /send <pid> <text>".into()));
                    }
                } else if line == "/end" {
                    // Tell the server to delete this session
                    let payload = serde_json::to_string(&ClientMessage::SessionEnd)?;
                    ws_send!(payload);
                    let _ = render_tx.send(RenderCmd::Notice(
                        "Session ended. Returning to session selection...".into(),
                    ));
                    let _ = render_tx.send(RenderCmd::Quit);
                    drop(render_tx);
                    let _ = term_handle.join();
                    return Ok(SessionResult::EndSession);
                } else if line == "/exit" {
                    let _ = render_tx.send(RenderCmd::Notice(
                        "Disconnecting. Session preserved. Returning to session selection..."
                            .into(),
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
                            let payload = serde_json::to_string(&ClientMessage::SessionRename {
                                name: name.to_string(),
                            })?;
                            ws_send!(payload);
                        }
                    } else if let Some(path) = rest.strip_prefix("workdir ") {
                        let path = path.trim();
                        if path.is_empty() {
                            let _ = render_tx
                                .send(RenderCmd::Error("Usage: /session workdir <path>".into()));
                        } else {
                            let payload = serde_json::to_string(&ClientMessage::SessionWorkdir {
                                path: path.to_string(),
                            })?;
                            ws_send!(payload);
                        }
                    } else {
                        let _ = render_tx.send(RenderCmd::Error(
                            "Usage: /session name <session name> OR /session workdir <path>".into(),
                        ));
                    }
                } else if line == "/relay" {
                    // Local-only slash command — show relay status
                    let relay_exists = bear_core::RelayConfig::exists();
                    let cfg = bear_core::ConfigFile::load();
                    let disabled = cfg.relay_disabled == Some(true);
                    let status_msg = if disabled {
                        "Relay: disabled (use `bear --enable-relay` to re-enable)".to_string()
                    } else if relay_exists {
                        if let Some(rc) = bear_core::RelayConfig::load() {
                            format!(
                                "Relay: configured\n  URL: {}\n  Room: {}\n  Use `bear --disable-relay` to disable",
                                rc.relay_url, rc.room_id
                            )
                        } else {
                            "Relay: relay.json exists but is invalid".to_string()
                        }
                    } else {
                        "Relay: not configured (use `bear --relay-pair <invite_code>` to set up)"
                            .to_string()
                    };
                    let _ = render_tx.send(RenderCmd::Notice(status_msg));
                } else if line == "/help" {
                    let command_lines = if slash_commands.is_empty() {
                        vec!["  (commands not loaded yet)".to_string()]
                    } else {
                        slash_commands
                            .iter()
                            .map(|(cmd, desc)| format!("  {cmd:<20} {desc}"))
                            .collect()
                    };
                    let mut help_lines = vec!["Commands:".to_string()];
                    help_lines.extend(command_lines);
                    help_lines.push(format!(
                        "  {:<20} {}",
                        "/relay", "Show relay status and config"
                    ));
                    help_lines.extend(
                        [
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
                        .iter()
                        .map(|s| s.to_string()),
                    );
                    let help = help_lines.join("\n");
                    let _ = render_tx.send(RenderCmd::Notice(help));
                } else if let Some(cmd) = line.strip_prefix('!') {
                    // Direct shell execution -> send ShellExec to server
                    let cmd = cmd.trim().to_string();
                    if cmd.is_empty() {
                        let _ = render_tx.send(RenderCmd::Error("Usage: !<command>".into()));
                    } else {
                        let _ = render_tx.send(RenderCmd::SuppressNextInputEcho);
                        let payload =
                            serde_json::to_string(&ClientMessage::ShellExec { command: cmd })?;
                        ws_send!(payload);
                    }
                } else {
                    // Regular chat input -> send to server/LLM
                    let _ = render_tx.send(RenderCmd::SuppressNextInputEcho);
                    let payload = serde_json::to_string(&ClientMessage::Input { text: line })?;
                    ws_send!(payload);
                }
            }
            LoopEvent::FromTerm(TermEvent::UserPromptResult {
                prompt_id,
                selected,
            }) => {
                let payload = serde_json::to_string(&ClientMessage::UserPromptResponse {
                    prompt_id,
                    selected,
                })?;
                ws_send!(payload);
            }
            LoopEvent::FromTerm(TermEvent::TaskPlanResult { plan_id, approved }) => {
                let payload =
                    serde_json::to_string(&ClientMessage::TaskPlanResponse { plan_id, approved })?;
                ws_send!(payload);
            }
            LoopEvent::FromTerm(TermEvent::Quit) => {
                let _ = render_tx.send(RenderCmd::Quit);
                drop(render_tx);
                let _ = term_handle.join();
                return Ok(SessionResult::Quit);
            }
        }
    }

    // Show disconnection message and return to session selection
    if disconnected {
        let _ = render_tx.send(RenderCmd::Error("Disconnected from server.".into()));
        let _ = render_tx.send(RenderCmd::Notice(
            "Session preserved. Returning to session selection...".into(),
        ));
        let _ = render_tx.send(RenderCmd::Quit);
        drop(render_tx);
        let _ = term_handle.join();
        return Ok(SessionResult::EndSession);
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
            let display_name = session
                .name
                .clone()
                .unwrap_or_else(|| session.id.to_string());
            let _ = render_tx.send(RenderCmd::SessionInfo(display_name, session.cwd.clone()));
        }
        ServerMessage::SlashCommands { commands } => {
            let list: Vec<(String, String)> = commands
                .iter()
                .map(|cmd| (cmd.cmd.clone(), cmd.desc.clone()))
                .collect();
            *slash_commands = list.clone();
            let _ = render_tx.send(RenderCmd::SlashCommands(list));
        }
        ServerMessage::AssistantText { text } => {
            let _ = render_tx.send(RenderCmd::AssistantChunk(text.clone()));
        }
        ServerMessage::ToolRequest {
            tool_call,
            extracted_commands,
        } => {
            *last_tool = (tool_call.name.clone(), tool_call.arguments.clone());
            let _ = render_tx.send(RenderCmd::ToolRequest {
                tool_call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                args: serde_json::to_string_pretty(&tool_call.arguments)
                    .unwrap_or_else(|_| tool_call.arguments.to_string()),
                extracted_commands: extracted_commands.clone().unwrap_or_default(),
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
        ServerMessage::ToolOutput {
            tool_name,
            tool_args,
            output,
            ..
        } => {
            *last_tool = (tool_name.clone(), tool_args.clone());
            let _ = render_tx.send(RenderCmd::ToolOutput {
                tool_name: tool_name.clone(),
                tool_args: tool_args.clone(),
                output: output.clone(),
            });
        }
        ServerMessage::ProcessStarted { info } => {
            let _ = render_tx.send(RenderCmd::ProcessEvent(format!(
                "Started pid={} cmd={}",
                info.pid, info.command
            )));
        }
        ServerMessage::ProcessOutput { pid, text } => {
            let _ = render_tx.send(RenderCmd::ProcessEvent(format!("[{}] {}", pid, text)));
        }
        ServerMessage::ProcessExited { pid, code } => {
            let code_str = code.map(|c| c.to_string()).unwrap_or("unknown".into());
            let _ = render_tx.send(RenderCmd::ProcessEvent(format!(
                "Process {} exited (code {})",
                pid, code_str
            )));
        }
        ServerMessage::ProcessListResult { processes } => {
            if processes.is_empty() {
                let _ = render_tx.send(RenderCmd::Notice("No background processes.".into()));
            } else {
                let mut lines = vec!["Background processes:".to_string()];
                for p in processes {
                    let status = if p.running { "running" } else { "exited" };
                    lines.push(format!("  pid={} [{}] {}", p.pid, status, p.command));
                }
                let _ = render_tx.send(RenderCmd::Notice(lines.join("\n")));
            }
        }
        ServerMessage::UserPrompt {
            prompt_id,
            question,
            options,
            multi,
        } => {
            let _ = render_tx.send(RenderCmd::UserPrompt {
                prompt_id: prompt_id.clone(),
                question: question.clone(),
                options: options.clone(),
                multi: *multi,
            });
        }
        ServerMessage::SessionRenamed { name } => {
            let _ = render_tx.send(RenderCmd::Notice(format!("Session renamed to: {name}")));
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
            let task_tuples: Vec<(String, String, bool)> = tasks
                .iter()
                .map(|t| (t.id.clone(), t.description.clone(), t.needs_write))
                .collect();
            let _ = render_tx.send(RenderCmd::TaskPlan {
                plan_id: plan_id.clone(),
                tasks: task_tuples,
            });
        }
        ServerMessage::TaskProgress {
            task_id,
            status,
            detail,
            ..
        } => {
            let _ = render_tx.send(RenderCmd::TaskProgress {
                task_id: task_id.clone(),
                status: status.clone(),
                detail: detail.clone(),
            });
        }
        ServerMessage::SubagentUpdate {
            subagent_id,
            description,
            status,
            detail,
        } => {
            let _ = render_tx.send(RenderCmd::SubagentUpdate {
                subagent_id: subagent_id.clone(),
                description: description.clone(),
                status: status.clone(),
                detail: detail.clone(),
            });
        }
        ServerMessage::ToolResolved {
            tool_call_id,
            approved,
        } => {
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
        ServerMessage::UserInput { text } => {
            let _ = render_tx.send(RenderCmd::UserInput { text: text.clone() });
        }
        ServerMessage::PlanUpdate {
            name,
            title,
            status,
            steps,
        } => {
            let step_tuples: Vec<(String, String, String, Option<String>)> = steps
                .iter()
                .map(|s| {
                    (
                        s.id.clone(),
                        s.description.clone(),
                        s.status.clone(),
                        s.detail.clone(),
                    )
                })
                .collect();
            let _ = render_tx.send(RenderCmd::PlanUpdate {
                _name: name.clone(),
                title: title.clone(),
                status: status.clone(),
                steps: step_tuples,
            });
        }
        ServerMessage::RelayStatus { status, detail } => {
            let msg = match detail {
                Some(d) => format!("Relay: {} ({})", status, d),
                None => format!("Relay: {}", status),
            };
            let _ = render_tx.send(RenderCmd::Notice(msg));
        }
        ServerMessage::ReplayStart { .. } => {
            let _ = render_tx.send(RenderCmd::ReplayStart);
        }
        ServerMessage::ReplayEnd => {
            let _ = render_tx.send(RenderCmd::ReplayEnd);
        }
        ServerMessage::Pong => {}
        // These are only used by the browser client (DataChannel lobby)
        ServerMessage::SessionListResult { .. } => {}
        ServerMessage::SessionCreated { .. } => {}
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
