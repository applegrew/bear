use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, ClearType},
};
use std::io::{self, Write};
use std::sync::mpsc as std_mpsc;

// ---------------------------------------------------------------------------
// Messages the terminal thread can receive
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RenderCmd {
    AssistantChunk(String),
    AssistantDone,
    Notice(String),
    Error(String),
    ToolRequest {
        tool_call_id: String,
        name: String,
        args: String,
    },
    /// Another client resolved this tool confirmation — dismiss picker.
    ToolResolved {
        tool_call_id: String,
        approved: bool,
    },
    /// Another client resolved this prompt — dismiss picker.
    PromptResolved {
        prompt_id: String,
    },
    ToolOutput {
        tool_name: String,
        tool_args: serde_json::Value,
        output: String,
    },
    ProcessEvent(String),
    SessionInfo(String, String),
    SlashCommands(Vec<(String, String)>),
    UserPrompt {
        prompt_id: String,
        question: String,
        options: Vec<String>,
        multi: bool,
    },
    SessionRenamed(String),
    ClientState {
        input_history: Vec<String>,
    },
    TaskPlan {
        plan_id: String,
        tasks: Vec<(String, String, bool)>, // (id, description, needs_write)
    },
    TaskProgress {
        task_id: String,
        status: String,
        detail: Option<String>,
    },
    SubagentUpdate {
        subagent_id: String,
        description: String,
        status: String,
        detail: Option<String>,
    },
    PlanUpdate {
        _name: String,
        title: String,
        status: String,
        steps: Vec<(String, String, String, Option<String>)>, // (id, description, status, detail)
    },
    Thinking,
    /// Another client submitted a chat prompt — display it.
    UserInput {
        text: String,
    },
    /// Tell the terminal to skip the next UserInput echo (we already rendered it locally).
    SuppressNextInputEcho,
    Quit,
}

pub fn cleanup_terminal() {
    let mut out = io::stdout();
    let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

// ---------------------------------------------------------------------------
// Events the terminal thread sends out
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolConfirmChoice {
    Approve,
    Deny,
    Always,
}

pub enum TermEvent {
    UserLine(String),
    ToolConfirmResult {
        tool_call_id: String,
        choice: ToolConfirmChoice,
    },
    UserPromptResult {
        prompt_id: String,
        selected: Vec<usize>,
    },
    TaskPlanResult {
        plan_id: String,
        approved: bool,
    },
    Quit,
}

// ---------------------------------------------------------------------------
// Spinner
// ---------------------------------------------------------------------------

const SPINNER_DOTS: &[&str] = &[
    "·····",
    "•····",
    "·•···",
    "··•··",
    "···•·",
    "····•",
    "·····",
];

// Layout constants
const INPUT_BOX_HEIGHT: u16 = 3; // top border + input line + bottom border
const STATUS_BAR_HEIGHT: u16 = 1;
const BOTTOM_RESERVE: u16 = INPUT_BOX_HEIGHT + STATUS_BAR_HEIGHT;

// ---------------------------------------------------------------------------
// Terminal thread
// ---------------------------------------------------------------------------

pub fn spawn_terminal_thread(
    render_rx: std_mpsc::Receiver<RenderCmd>,
    event_tx: tokio::sync::mpsc::Sender<TermEvent>,
) -> std::thread::JoinHandle<()> {
    let rt = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut state = match TermState::init() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to initialize terminal: {e}");
                return;
            }
        };

        // Welcome banner
        state.push_line("");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⣤⠶⣤⣤⣤⡴⢦⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⢷⠉⠀⠀⠀⠈⠁⢷⠖⠒⠲⠶⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⢸⠀⠠⠀⠠⠀⢠⠀⢳⠀⠀⠀⠀⠉⢳⡄⠀⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⠈⣧⡀⣶⡆⣠⠏⠀⠀⠀⠀⠀⠀⠀⠀⢹⡄⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⠀⢸⡉⠒⠚⠁⠀⠀⠀⢀⠀⠀⠀⠀⠀⠀⡇⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⠀⠸⡇⢢⡀⣤⠀⠀⠀⢸⠀⠀⠀⠀⠀⢀⡇⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⠀⠀⣷⠀⠉⢻⡀⠀⠀⣾⠤⠤⡄⠀⠀⢸⠁⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⠀⠀⣸⠄⠀⣼⡇⠀⢠⡇⢀⡼⣻⠀⢀⡟⠀⠀⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("\x1b[33m  ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠛⠒⠚⠙⠷⠶⠞⠉⠉⠀⠓⠒⠚⠁⠀⠀⠀⠀⠀\x1b[0m");
        state.push_line("");
        state.push_line("\x1b[1m\x1b[36m    Welcome to Bear coding agent\x1b[0m");
        state.push_line("\x1b[90m    Type /help for commands\x1b[0m");
        state.push_line("");

        state.full_repaint();

        loop {
            // Drain render commands
            loop {
                // Process any deferred commands from a previous blocking picker first
                let next_cmd = if !state.deferred_cmds.is_empty() {
                    Some(state.deferred_cmds.remove(0))
                } else {
                    match render_rx.try_recv() {
                        Ok(cmd) => Some(cmd),
                        Err(std_mpsc::TryRecvError::Empty) => None,
                        Err(std_mpsc::TryRecvError::Disconnected) => {
                            state.cleanup();
                            return;
                        }
                    }
                };

                let Some(cmd) = next_cmd else { break };

                if matches!(cmd, RenderCmd::Quit) {
                    state.cleanup();
                    return;
                }
                if let RenderCmd::ToolRequest {
                    tool_call_id,
                    name,
                    args,
                    ..
                } = cmd
                {
                    if let Some(choice) =
                        state.run_tool_confirm_picker(&render_rx, &tool_call_id, &name, &args)
                    {
                        let _ = rt.block_on(event_tx.send(TermEvent::ToolConfirmResult {
                            tool_call_id,
                            choice,
                        }));
                    }
                    if state.quit_requested {
                        state.cleanup();
                        let _ = rt.block_on(event_tx.send(TermEvent::Quit));
                        return;
                    }
                    // else: resolved by another client, no event to send
                    state.full_repaint();
                    continue;
                }
                if let RenderCmd::UserPrompt {
                    prompt_id,
                    question,
                    options,
                    multi,
                } = cmd
                {
                    if let Some(selected) = state.run_inline_menu(
                        &render_rx,
                        Some(&prompt_id),
                        &question,
                        &options,
                        multi,
                    ) {
                        let _ = rt.block_on(event_tx.send(TermEvent::UserPromptResult {
                            prompt_id,
                            selected,
                        }));
                    }
                    if state.quit_requested {
                        state.cleanup();
                        let _ = rt.block_on(event_tx.send(TermEvent::Quit));
                        return;
                    }
                    state.full_repaint();
                    continue;
                }
                if let RenderCmd::TaskPlan { plan_id, tasks } = cmd {
                    // Render the plan
                    state.push_line("");
                    state.push_line(&format!("  {} Proposed task plan:", a_cyan("📋")));
                    for (id, desc, needs_write) in &tasks {
                        let tag = if *needs_write {
                            a_yellow("[write]")
                        } else {
                            a_green("[read]")
                        };
                        state.push_line(&format!(
                            "    {} {} {}",
                            a_gray(&format!("{}.", id)),
                            tag,
                            desc
                        ));
                    }
                    state.push_line("");
                    // Use the existing inline menu for approval
                    let options = vec!["Approve".to_string(), "Reject".to_string()];
                    if let Some(selected) = state.run_inline_menu(
                        &render_rx,
                        Some(&plan_id),
                        "Execute this plan?",
                        &options,
                        false,
                    ) {
                        let approved = selected.first().copied() == Some(0);
                        let _ = rt.block_on(
                            event_tx.send(TermEvent::TaskPlanResult { plan_id, approved }),
                        );
                    }
                    if state.quit_requested {
                        state.cleanup();
                        let _ = rt.block_on(event_tx.send(TermEvent::Quit));
                        return;
                    }
                    state.full_repaint();
                    continue;
                }
                // Ignore stale resolution messages that arrive outside of a picker
                if matches!(
                    cmd,
                    RenderCmd::ToolResolved { .. } | RenderCmd::PromptResolved { .. }
                ) {
                    continue;
                }
                state.handle_render(cmd);
            }

            // Advance spinner / update warning countdown
            {
                let now = std::time::Instant::now();
                let needs_status_redraw = state.interrupt_warning_remaining_ms() > 0;
                if state.streaming || needs_status_redraw {
                    if now.duration_since(state.spinner_last_tick)
                        >= std::time::Duration::from_millis(100)
                    {
                        state.spinner_frame = (state.spinner_frame + 1) % SPINNER_DOTS.len();
                        state.spinner_last_tick = now;
                        // Auto-dismiss expired warning
                        if state.interrupt_warning_start.is_some()
                            && state.interrupt_warning_remaining_ms() == 0
                        {
                            state.dismiss_interrupt_warning();
                        }
                        state.draw_status_bar();
                        let _ = io::stdout().flush();
                    }
                }
            }

            // Poll terminal events (50ms)
            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                let ev = event::read();
                if let Ok(Event::Key(key)) = ev {
                    let action = map_key(key);

                    if state.dropdown_active() {
                        match action {
                            KeyAction::HistoryPrev => {
                                state.dropdown_up();
                                state.draw_input_box();
                                continue;
                            }
                            KeyAction::HistoryNext => {
                                state.dropdown_down();
                                state.draw_input_box();
                                continue;
                            }
                            KeyAction::Tab => {
                                state.accept_dropdown();
                                state.draw_input_box();
                                continue;
                            }
                            KeyAction::Submit => {
                                if state.dropdown_idx.is_some() {
                                    state.accept_dropdown();
                                    state.draw_input_box();
                                    continue;
                                }
                            }
                            KeyAction::Escape => {
                                state.input_buf.clear();
                                state.cursor_pos = 0;
                                state.dropdown_idx = None;
                                state.draw_input_box();
                                continue;
                            }
                            _ => {
                                state.dropdown_idx = None;
                            }
                        }
                    }

                    match action {
                        KeyAction::Char(c) => {
                            // Typing dismisses any active interrupt warning
                            if state.interrupt_pending_text.is_some() {
                                state.dismiss_interrupt_warning();
                            }
                            state.insert_char(c);
                            state.draw_input_box();
                        }
                        KeyAction::Backspace => {
                            if state.interrupt_pending_text.is_some() {
                                state.dismiss_interrupt_warning();
                            }
                            state.backspace();
                            state.draw_input_box();
                        }
                        KeyAction::Delete => {
                            if state.interrupt_pending_text.is_some() {
                                state.dismiss_interrupt_warning();
                            }
                            state.delete();
                            state.draw_input_box();
                        }
                        KeyAction::Left => {
                            state.cursor_left();
                            state.draw_input_box();
                        }
                        KeyAction::Right => {
                            state.cursor_right();
                            state.draw_input_box();
                        }
                        KeyAction::Home => {
                            state.cursor_pos = 0;
                            state.draw_input_box();
                        }
                        KeyAction::End => {
                            state.cursor_pos = state.input_buf.len();
                            state.draw_input_box();
                        }
                        KeyAction::HistoryPrev => {
                            state.history_prev();
                            state.draw_input_box();
                        }
                        KeyAction::HistoryNext => {
                            state.history_next();
                            state.draw_input_box();
                        }
                        KeyAction::ScrollUp => {
                            state.scroll_up(3);
                            state.full_repaint();
                        }
                        KeyAction::ScrollDown => {
                            state.scroll_down(3);
                            state.full_repaint();
                        }
                        KeyAction::Tab => {}
                        KeyAction::Escape => {}
                        KeyAction::Submit => {
                            // If interrupt warning is active and user presses Enter again, confirm interrupt
                            if state.interrupt_pending_text.is_some()
                                && state.interrupt_warning_remaining_ms() > 0
                            {
                                let text = state.interrupt_pending_text.take().unwrap();
                                state.interrupt_warning_start = None;
                                let _ = rt.block_on(event_tx.send(TermEvent::UserLine(text)));
                                state.full_repaint();
                            } else if state.streaming
                                && !state.input_buf.trim().is_empty()
                                && !state.input_buf.trim().starts_with('/')
                            {
                                // LLM is busy and user submitted non-empty text: show warning
                                // (slash commands bypass this — they don't go to the LLM)
                                let text = state.submit();
                                state.interrupt_pending_text = Some(text);
                                state.interrupt_warning_start = Some(std::time::Instant::now());
                                state.draw_status_bar();
                            } else {
                                // Normal submit (LLM idle, empty input, or slash command)
                                state.dismiss_interrupt_warning();
                                let line = state.submit();
                                let _ = rt.block_on(event_tx.send(TermEvent::UserLine(line)));
                                state.full_repaint();
                            }
                        }
                        KeyAction::Quit => {
                            state.cleanup();
                            let _ = rt.block_on(event_tx.send(TermEvent::Quit));
                            return;
                        }
                        KeyAction::None => {}
                    }
                }
            }

            // Handle resize
            if let Ok((w, h)) = terminal::size() {
                if w != state.term_width || h != state.term_height {
                    state.term_width = w;
                    state.term_height = h;
                    state.full_repaint();
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct TermState {
    input_buf: String,
    cursor_pos: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    saved_input: String,

    output_lines: Vec<String>,
    scroll_offset: usize,

    streaming: bool,
    streaming_buf: String,
    thinking_line_shown: bool,

    spinner_frame: usize,
    spinner_last_tick: std::time::Instant,

    session_name: String,
    session_cwd: String,

    term_width: u16,
    term_height: u16,

    dropdown_idx: Option<usize>,
    last_dropdown_count: usize,
    slash_commands: Vec<(String, String)>,

    /// Commands received during a blocking picker that need to be processed later.
    deferred_cmds: Vec<RenderCmd>,

    /// Set by a blocking picker when the user presses Ctrl+C to quit.
    quit_requested: bool,

    /// Set after this client submits input, cleared when the echo arrives.
    /// Prevents double-rendering of our own prompt.
    awaiting_input_echo: bool,

    /// When the user submits while LLM is busy, the text is buffered here.
    /// A second Enter within the countdown window will actually send it.
    interrupt_pending_text: Option<String>,
    /// When the interrupt warning was first shown (for the 6s countdown).
    interrupt_warning_start: Option<std::time::Instant>,

    /// Track active subagent count by subagent_id.
    active_subagents: std::collections::HashSet<String>,
}

/// Format a tool call into human-readable description lines for the card UI.
pub fn format_tool_description(name: &str, args: &serde_json::Value) -> Vec<String> {
    match name {
        "run_command" => {
            let cmd = args["command"].as_str().unwrap_or("(unknown)");
            vec![format!("$ {cmd}")]
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            vec![format!("Reading: {path}")]
        }
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            vec![format!("Writing: {path}")]
        }
        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            let find = args["find"].as_str().unwrap_or("");
            let preview = if find.len() > 60 {
                let end = find.floor_char_boundary(60);
                &find[..end]
            } else {
                find
            };
            vec![format!("Editing: {path}"), format!("Find: {preview}…")]
        }
        "patch_file" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            vec![format!("Patching: {path}")]
        }
        "read_symbol" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            let symbol = args["symbol"].as_str().unwrap_or("(unknown)");
            vec![format!("Reading symbol: {symbol} in {path}")]
        }
        "patch_symbol" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            let symbol = args["symbol"].as_str().unwrap_or("(unknown)");
            vec![format!("Patching symbol: {symbol} in {path}")]
        }
        "list_files" => {
            let path = args["path"].as_str().unwrap_or(".");
            let glob = args["glob"].as_str().unwrap_or("*");
            vec![format!("Listing: {path}  (glob: {glob})")]
        }
        "search_text" => {
            let pattern = args["pattern"].as_str().unwrap_or("(unknown)");
            let path = args["path"].as_str().unwrap_or(".");
            vec![format!("Searching: \"{pattern}\" in {path}")]
        }
        "undo" => {
            let steps = args["steps"].as_u64().unwrap_or(1);
            vec![format!("Undo {steps} step(s)")]
        }
        _ => {
            if let Some(obj) = args.as_object() {
                obj.iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(s) => {
                                if s.len() > 60 {
                                    format!("{}…", &s[..60])
                                } else {
                                    s.clone()
                                }
                            }
                            other => {
                                let s = other.to_string();
                                if s.len() > 60 {
                                    format!("{}…", &s[..60])
                                } else {
                                    s
                                }
                            }
                        };
                        format!("{k}: {val}")
                    })
                    .collect()
            } else {
                vec![args.to_string()]
            }
        }
    }
}

/// Compute visible length stripping ANSI escapes.
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Wrap a line into multiple visual rows of at most `max` visible characters,
/// preserving ANSI escape codes across wraps.
fn wrap_visible(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![s.to_string()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut vis = 0;
    let mut in_esc = false;
    // Track the "active" ANSI state so we can re-apply it on continuation lines
    let mut active_ansi: Vec<String> = Vec::new();

    for c in s.chars() {
        if in_esc {
            current.push(c);
            if c.is_ascii_alphabetic() {
                in_esc = false;
                // Track the escape sequence we just finished
                // Find the start of this escape sequence
                if let Some(esc_start) = current.rfind('\x1b') {
                    let seq = current[esc_start..].to_string();
                    if seq == "\x1b[0m" || seq == "\x1b[m" {
                        active_ansi.clear();
                    } else {
                        active_ansi.push(seq);
                    }
                }
            }
        } else if c == '\x1b' {
            in_esc = true;
            current.push(c);
        } else {
            if vis >= max {
                // End current row with a reset
                current.push_str("\x1b[0m");
                rows.push(current);
                // Start new row, re-apply active ANSI state
                current = active_ansi.join("");
                vis = 0;
            }
            current.push(c);
            vis += 1;
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

// ANSI helpers
fn a_green(s: &str) -> String {
    format!("\x1b[38;5;114m{s}\x1b[0m")
}
fn a_red(s: &str) -> String {
    format!("\x1b[38;5;204m{s}\x1b[0m")
}
fn a_yellow(s: &str) -> String {
    format!("\x1b[38;5;180m{s}\x1b[0m")
}
fn a_cyan(s: &str) -> String {
    format!("\x1b[38;5;80m{s}\x1b[0m")
}
fn a_gray(s: &str) -> String {
    format!("\x1b[38;5;102m{s}\x1b[0m")
}
fn a_white(s: &str) -> String {
    format!("\x1b[38;5;252m{s}\x1b[0m")
}
fn a_magenta(s: &str) -> String {
    format!("\x1b[38;5;141m{s}\x1b[0m")
}
fn a_bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[0m")
}
fn a_dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

// ---------------------------------------------------------------------------
// Markdown → ANSI rendering
// ---------------------------------------------------------------------------

/// ANSI escape constants for markdown rendering
const MD_RESET: &str = "\x1b[0m";
const MD_BOLD: &str = "\x1b[1m";
const MD_ITALIC: &str = "\x1b[3m";
const MD_H1: &str = "\x1b[1m\x1b[38;5;80m"; // bold cyan
const MD_H2: &str = "\x1b[1m\x1b[38;5;114m"; // bold green
const MD_H3: &str = "\x1b[1m\x1b[38;5;180m"; // bold yellow
const MD_CODE_INLINE: &str = "\x1b[38;5;222m"; // warm yellow for `code`
const MD_CODE_BLOCK: &str = "\x1b[38;5;246m"; // light gray for code blocks
const MD_CODE_LANG: &str = "\x1b[38;5;102m"; // dim gray for language tag
const MD_BULLET: &str = "\x1b[38;5;80m"; // cyan bullets
const MD_HRULE: &str = "\x1b[38;5;240m"; // dim horizontal rule
const MD_LINK: &str = "\x1b[4m\x1b[38;5;75m"; // underline blue for links
const MD_GREEN: &str = "\x1b[38;5;114m"; // default text green

/// Render a block of markdown text into ANSI-colored lines.
/// Returns a Vec of already-colored strings (one per output line).
/// `in_code_block` tracks state across calls for streaming.
fn render_md_lines(text: &str, in_code_block: &mut bool) -> Vec<String> {
    let mut result = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Code block fences
        if trimmed.starts_with("```") {
            if *in_code_block {
                // Closing fence
                *in_code_block = false;
                result.push(format!("{MD_CODE_BLOCK}```{MD_RESET}"));
            } else {
                // Opening fence — show language tag if present
                *in_code_block = true;
                let lang = trimmed.trim_start_matches('`').trim();
                if lang.is_empty() {
                    result.push(format!("{MD_CODE_BLOCK}```{MD_RESET}"));
                } else {
                    result.push(format!("{MD_CODE_BLOCK}```{MD_CODE_LANG}{lang}{MD_RESET}"));
                }
            }
            continue;
        }

        // Inside code block — render as-is in code color, no inline formatting
        if *in_code_block {
            result.push(format!("{MD_CODE_BLOCK}{line}{MD_RESET}"));
            continue;
        }

        // Horizontal rule
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            result.push(format!("{MD_HRULE}─────────────────────────────{MD_RESET}"));
            continue;
        }

        // Headers
        if trimmed.starts_with("### ") {
            let content = &trimmed[4..];
            result.push(format!("{MD_H3}{content}{MD_RESET}"));
            continue;
        }
        if trimmed.starts_with("## ") {
            let content = &trimmed[3..];
            result.push(format!("{MD_H2}{content}{MD_RESET}"));
            continue;
        }
        if trimmed.starts_with("# ") {
            let content = &trimmed[2..];
            result.push(format!("{MD_H1}{content}{MD_RESET}"));
            continue;
        }

        // List items: -, *, or numbered (1. 2. etc.)
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let formatted = render_md_inline(rest);
            result.push(format!("{MD_BULLET}  • {MD_GREEN}{formatted}{MD_RESET}"));
            continue;
        }
        // Numbered list
        if let Some(dot_pos) = trimmed.find(". ") {
            let num_part = &trimmed[..dot_pos];
            if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
                let rest = &trimmed[dot_pos + 2..];
                let formatted = render_md_inline(rest);
                result.push(format!(
                    "{MD_BULLET}  {num_part}. {MD_GREEN}{formatted}{MD_RESET}"
                ));
                continue;
            }
        }

        // Regular line — apply inline formatting
        if trimmed.is_empty() {
            result.push(String::new());
        } else {
            let formatted = render_md_inline(line);
            result.push(format!("{MD_GREEN}{formatted}{MD_RESET}"));
        }
    }

    result
}

/// Apply inline markdown formatting: **bold**, *italic*, `code`, [links](url)
fn render_md_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(MD_BOLD);
                out.push_str(&inner);
                out.push_str(MD_RESET);
                out.push_str(MD_GREEN);
                i = end + 2;
                continue;
            }
        }

        // Italic: *text* (single asterisk, not followed by another)
        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(MD_ITALIC);
                out.push_str(&inner);
                out.push_str(MD_RESET);
                out.push_str(MD_GREEN);
                i = end + 1;
                continue;
            }
        }

        // Inline code: `code`
        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(MD_CODE_INLINE);
                out.push_str(&inner);
                out.push_str(MD_RESET);
                out.push_str(MD_GREEN);
                i = end + 1;
                continue;
            }
        }

        // Link: [text](url)
        if chars[i] == '[' {
            if let Some(close_bracket) = find_closing_single(&chars, i + 1, ']') {
                if close_bracket + 1 < len && chars[close_bracket + 1] == '(' {
                    if let Some(close_paren) = find_closing_single(&chars, close_bracket + 2, ')') {
                        let link_text: String = chars[i + 1..close_bracket].iter().collect();
                        out.push_str(MD_LINK);
                        out.push_str(&link_text);
                        out.push_str(MD_RESET);
                        out.push_str(MD_GREEN);
                        i = close_paren + 1;
                        continue;
                    }
                }
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Find closing double-char delimiter (e.g. **) starting from `start`.
fn find_closing(chars: &[char], start: usize, delim: &[char; 2]) -> Option<usize> {
    let len = chars.len();
    let mut j = start;
    while j + 1 < len {
        if chars[j] == delim[0] && chars[j + 1] == delim[1] {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Find closing single-char delimiter starting from `start`.
fn find_closing_single(chars: &[char], start: usize, delim: char) -> Option<usize> {
    for j in start..chars.len() {
        if chars[j] == delim {
            return Some(j);
        }
    }
    None
}

const DISPLAY_MAX_LINES: usize = 20;

impl TermState {
    fn init() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let mut out = io::stdout();
        let _ = execute!(out, terminal::EnterAlternateScreen, cursor::Hide);
        Ok(Self {
            input_buf: String::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            output_lines: Vec::new(),
            scroll_offset: 0,
            streaming: false,
            streaming_buf: String::new(),
            thinking_line_shown: false,
            spinner_frame: 0,
            spinner_last_tick: std::time::Instant::now(),
            session_name: String::new(),
            session_cwd: String::new(),
            term_width: w,
            term_height: h,
            dropdown_idx: None,
            last_dropdown_count: 0,
            slash_commands: Vec::new(),
            deferred_cmds: Vec::new(),
            quit_requested: false,
            awaiting_input_echo: false,
            interrupt_pending_text: None,
            interrupt_warning_start: None,
            active_subagents: std::collections::HashSet::new(),
        })
    }

    fn cleanup(&self) {
        cleanup_terminal();
    }

    fn push_line(&mut self, s: &str) {
        self.output_lines.push(s.to_string());
        self.scroll_offset = 0;
    }

    fn push_lines(&mut self, lines: Vec<String>) {
        self.output_lines.extend(lines);
        self.scroll_offset = 0;
    }

    fn output_area_height(&self) -> usize {
        self.term_height.saturating_sub(BOTTOM_RESERVE) as usize
    }

    fn scroll_up(&mut self, n: usize) {
        let max = self
            .output_lines
            .len()
            .saturating_sub(self.output_area_height());
        self.scroll_offset = (self.scroll_offset + n).min(max);
    }

    fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    // -----------------------------------------------------------------------
    // Full repaint
    // -----------------------------------------------------------------------

    fn full_repaint(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(out, cursor::Hide);
        // Each draw_* method clears its own rows individually, so no full-screen
        // clear is needed. Avoiding ClearType::All preserves terminal scrollback.
        self.draw_output_area();
        self.draw_input_box();
        self.draw_status_bar();
        let _ = out.flush();
    }

    fn draw_output_area(&mut self) {
        let mut out = io::stdout();
        let height = self.output_area_height();
        let width = self.term_width as usize;
        let total = self.output_lines.len();

        let end = total.saturating_sub(self.scroll_offset);
        let start = end.saturating_sub(height);

        // Collect wrapped visual rows from the output lines in the visible window
        let mut visual_rows: Vec<String> = Vec::new();
        for line_idx in start..end {
            let line = &self.output_lines[line_idx];
            // Strip internal STREAM tag before rendering
            let clean = if line.starts_with("\x01STREAM\x01") {
                &line["\x01STREAM\x01".len()..]
            } else {
                line.as_str()
            };
            let wrapped = wrap_visible(clean, width);
            for w in wrapped {
                visual_rows.push(w);
            }
        }

        // Only show the last `height` visual rows (scroll to bottom)
        let vr_start = visual_rows.len().saturating_sub(height);
        for row in 0..height {
            let _ = execute!(out, cursor::MoveTo(0, row as u16));
            let _ = execute!(out, terminal::Clear(ClearType::CurrentLine));
            let vr_idx = vr_start + row;
            if vr_idx < visual_rows.len() {
                let _ = execute!(out, Print(&visual_rows[vr_idx]), ResetColor);
            }
        }

        if self.scroll_offset > 0 {
            let indicator = format!(" ↑ {} more ", self.scroll_offset);
            let col = width.saturating_sub(visible_len(&indicator) + 1);
            let _ = execute!(
                out,
                cursor::MoveTo(col as u16, 0),
                SetForegroundColor(Color::Black),
                SetBackgroundColor(Color::DarkYellow),
                Print(&indicator),
                ResetColor,
            );
        }
    }

    fn draw_input_box(&mut self) {
        let mut out = io::stdout();
        // Hide cursor during intermediate draws to prevent flicker (M5)
        let _ = execute!(out, cursor::Hide);
        let width = self.term_width as usize;
        let input_row = self.term_height.saturating_sub(BOTTOM_RESERVE);
        let border_w = width.saturating_sub(2);

        // Top border
        let _ = execute!(
            out,
            cursor::MoveTo(0, input_row),
            terminal::Clear(ClearType::CurrentLine)
        );
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("┌"),
            Print("─".repeat(border_w)),
            Print("┐"),
            ResetColor,
        );

        // Input line
        let _ = execute!(
            out,
            cursor::MoveTo(0, input_row + 1),
            terminal::Clear(ClearType::CurrentLine)
        );
        let (prompt, prompt_color) = if self.input_buf.starts_with('/') {
            ("cmd-> ", Color::Yellow)
        } else if self.input_buf.starts_with('!') {
            ("shell>", Color::Magenta)
        } else {
            ("bear> ", Color::Cyan)
        };
        let inner_w = width.saturating_sub(4);
        let prompt_len = 6usize;
        let text_space = inner_w.saturating_sub(prompt_len);

        // Compute a scroll window that keeps the cursor visible (M4).
        // `scroll_start` is the char offset into input_buf where the visible
        // portion begins.  We work in chars to handle multi-byte correctly.
        let char_count = self.input_buf.chars().count();
        let cursor_char = self.cursor_pos.min(char_count);
        let scroll_start = if cursor_char < text_space {
            0
        } else {
            cursor_char - text_space + 1
        };
        let visible_chars: String = self
            .input_buf
            .chars()
            .skip(scroll_start)
            .take(text_space)
            .collect();
        let display_text = &visible_chars;
        let visible_char_count = visible_chars.chars().count();
        let padding = inner_w.saturating_sub(prompt_len + visible_char_count);

        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("│ "),
            SetForegroundColor(prompt_color),
            SetAttribute(Attribute::Bold),
            Print(prompt),
            SetAttribute(Attribute::Reset),
            ResetColor,
            Print(display_text),
            Print(" ".repeat(padding)),
            SetForegroundColor(Color::DarkGrey),
            Print(" │"),
            ResetColor,
        );

        // Bottom border
        let _ = execute!(
            out,
            cursor::MoveTo(0, input_row + 2),
            terminal::Clear(ClearType::CurrentLine)
        );
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("└"),
            Print("─".repeat(border_w)),
            Print("┘"),
            ResetColor,
        );

        // If dropdown was previously shown, redraw the output area to restore those rows
        if self.last_dropdown_count > 0 {
            self.last_dropdown_count = 0;
            self.draw_output_area();
        }

        // Cursor position: account for scroll offset (M4)
        let cursor_col = 2 + prompt_len + (cursor_char - scroll_start).min(text_space);
        let _ = execute!(
            out,
            cursor::MoveTo(cursor_col as u16, input_row + 1),
            cursor::Show
        );

        // Dropdown above input box
        if self.input_buf.starts_with('/') {
            let matches = self.matching_slash_commands(&self.input_buf);
            if !matches.is_empty() {
                if let Some(idx) = self.dropdown_idx {
                    if idx >= matches.len() {
                        self.dropdown_idx = Some(matches.len() - 1);
                    }
                }
                let dd_start = input_row.saturating_sub(matches.len() as u16);
                for (i, (cmd, desc)) in matches.iter().enumerate() {
                    let row = dd_start + i as u16;
                    let _ = execute!(
                        out,
                        cursor::MoveTo(0, row),
                        terminal::Clear(ClearType::CurrentLine)
                    );
                    let selected = self.dropdown_idx == Some(i);
                    if selected {
                        let _ = execute!(
                            out,
                            SetForegroundColor(Color::Yellow),
                            Print("  ❯ "),
                            SetForegroundColor(Color::White),
                            Print(cmd),
                            SetForegroundColor(Color::DarkGrey),
                            Print("  "),
                            Print(desc),
                            ResetColor,
                        );
                    } else {
                        let _ = execute!(
                            out,
                            SetForegroundColor(Color::DarkGrey),
                            Print("    "),
                            SetForegroundColor(Color::Yellow),
                            Print(cmd),
                            SetForegroundColor(Color::DarkGrey),
                            Print("  "),
                            Print(desc),
                            ResetColor,
                        );
                    }
                }
                self.last_dropdown_count = matches.len();
                let _ = execute!(out, cursor::MoveTo(cursor_col as u16, input_row + 1));
            }
        }

        let _ = out.flush();
    }

    fn draw_status_bar(&mut self) {
        let mut out = io::stdout();
        let row = self.term_height.saturating_sub(1);
        let width = self.term_width as usize;

        let _ = execute!(
            out,
            cursor::MoveTo(0, row),
            terminal::Clear(ClearType::CurrentLine)
        );

        // Check if interrupt warning is active
        let warning_active = self.interrupt_warning_remaining_ms() > 0;

        if warning_active {
            let remaining_ms = self.interrupt_warning_remaining_ms();
            let warn_text = "LLM is busy — press Enter again to interrupt";
            let warn_len = visible_len(warn_text);
            // Animated underline: full width at 6s, shrinks to 0
            let bar_max = warn_len;
            let bar_len =
                (bar_max as u64 * remaining_ms as u64 / 6000).min(bar_max as u64) as usize;

            let _ = execute!(
                out,
                cursor::MoveTo(1, row),
                SetForegroundColor(Color::Yellow),
                Print(warn_text),
                ResetColor,
            );
            // Shrinking underline bar: starts full-width, shrinks to 0 over 6 seconds
            if bar_len > 0 {
                let bar_start = width.saturating_sub(bar_len + 1);
                let _ = execute!(
                    out,
                    cursor::MoveTo(bar_start as u16, row),
                    SetForegroundColor(Color::DarkYellow),
                    Print("▁".repeat(bar_len)),
                    ResetColor,
                );
            }
        } else {
            let spinner = if self.streaming {
                SPINNER_DOTS[self.spinner_frame % SPINNER_DOTS.len()]
            } else {
                "·····"
            };

            let session = if self.session_name.is_empty() {
                "bear"
            } else {
                &self.session_name
            };

            // Left: spinner + session + subagent count
            let subagent_info = if self.active_subagents.is_empty() {
                String::new()
            } else {
                let n = self.active_subagents.len();
                if n == 1 {
                    "  [1 sub-agent]".to_string()
                } else {
                    format!("  [{n} sub-agents]")
                }
            };
            let left = format!("{spinner}  {session}{subagent_info}");
            // Right: shortcuts
            let right = "↑↓ history  ctrl+c quit";

            let left_len = visible_len(&left);
            let right_len = visible_len(right);
            let gap = width.saturating_sub(left_len + right_len + 2);

            let _ = execute!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(" "),
                Print(&left),
                Print(" ".repeat(gap)),
                Print(right),
                Print(" "),
                ResetColor,
            );
        }

        // Restore cursor to input box
        let input_row = self.term_height.saturating_sub(BOTTOM_RESERVE) + 1;
        let prompt_len = 6usize;
        let inner_w = width.saturating_sub(4);
        let text_space = inner_w.saturating_sub(prompt_len);
        let cursor_col = 2 + prompt_len + self.cursor_pos.min(text_space);
        let _ = execute!(
            out,
            cursor::MoveTo(cursor_col as u16, input_row),
            cursor::Show
        );
        let _ = out.flush();
    }

    /// Returns remaining milliseconds for the interrupt warning, or 0 if not active.
    fn interrupt_warning_remaining_ms(&self) -> u64 {
        match self.interrupt_warning_start {
            Some(start) => {
                let elapsed = start.elapsed().as_millis() as u64;
                if elapsed >= 6000 {
                    0
                } else {
                    6000 - elapsed
                }
            }
            None => 0,
        }
    }

    /// Dismiss the interrupt warning and clear pending text.
    fn dismiss_interrupt_warning(&mut self) {
        self.interrupt_pending_text = None;
        self.interrupt_warning_start = None;
    }

    // -----------------------------------------------------------------------
    // Dropdown helpers
    // -----------------------------------------------------------------------

    fn dropdown_active(&self) -> bool {
        self.input_buf.starts_with('/') && !self.matching_slash_commands(&self.input_buf).is_empty()
    }

    fn dropdown_up(&mut self) {
        let matches = self.matching_slash_commands(&self.input_buf);
        if matches.is_empty() {
            return;
        }
        self.dropdown_idx = Some(match self.dropdown_idx {
            None | Some(0) => matches.len() - 1,
            Some(i) => i - 1,
        });
    }

    fn dropdown_down(&mut self) {
        let matches = self.matching_slash_commands(&self.input_buf);
        if matches.is_empty() {
            return;
        }
        self.dropdown_idx = Some(match self.dropdown_idx {
            None => 0,
            Some(i) if i + 1 >= matches.len() => 0,
            Some(i) => i + 1,
        });
    }

    fn accept_dropdown(&mut self) {
        let matches = self.matching_slash_commands(&self.input_buf);
        let idx = self.dropdown_idx.unwrap_or(0);
        if let Some((cmd, _)) = matches.get(idx) {
            self.input_buf = format!("{} ", cmd);
            self.cursor_pos = self.input_buf.len();
        }
        self.dropdown_idx = None;
    }

    fn matching_slash_commands(&self, input: &str) -> Vec<(String, String)> {
        if !input.starts_with('/') {
            return Vec::new();
        }
        let typed = input.trim_end();
        self.slash_commands
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(typed) || typed.starts_with(cmd))
            .take(5)
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Tool output rendering (to output buffer)
    // -----------------------------------------------------------------------

    fn render_tool_output_to_buf(
        &self,
        tool_name: &str,
        tool_args: &serde_json::Value,
        output: &str,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        match tool_name {
            "read_file" => {
                let path = tool_args["path"].as_str().unwrap_or("?");
                let lc = output.lines().count();
                if output.starts_with("Error") {
                    lines.push(format!("  {} {}", a_red("✗"), a_red(output)));
                } else {
                    lines.push(format!(
                        "  {} {}",
                        a_green("✓"),
                        a_green(&format!("Read {path} ({lc} lines)"))
                    ));
                }
            }
            "write_file" | "edit_file" | "patch_file" => {
                let is_err = output.starts_with("Error") || output.starts_with("Patch failed");
                let (status, diff) = if let Some(pos) = output.find("\n\n") {
                    (&output[..pos], Some(output[pos + 2..].trim_end()))
                } else {
                    (output, None)
                };
                if is_err {
                    lines.push(format!("  {} {}", a_red("✗"), a_red(status)));
                } else {
                    lines.push(format!("  {} {}", a_green("✓"), a_green(status)));
                }
                if let Some(d) = diff {
                    lines.extend(Self::format_diff_lines(d));
                }
            }
            "run_command" => {
                lines.extend(Self::truncated_lines(output, DISPLAY_MAX_LINES));
            }
            "list_files" => {
                let count = output.lines().count();
                lines.push(format!(
                    "  {} {}",
                    a_green("✓"),
                    a_green(&format!("{count} entries"))
                ));
                lines.extend(Self::truncated_lines(output, DISPLAY_MAX_LINES));
            }
            "search_text" => {
                if output == "No matches found." {
                    lines.push(format!("  {} {}", a_gray("│"), a_gray(output)));
                } else {
                    let count = output
                        .lines()
                        .filter(|l| !l.starts_with('[') && !l.is_empty())
                        .count();
                    lines.push(format!(
                        "  {} {}",
                        a_green("✓"),
                        a_green(&format!("{count} matches"))
                    ));
                    lines.extend(Self::truncated_lines(output, DISPLAY_MAX_LINES));
                }
            }
            "undo" => {
                if output.starts_with("Error") || output == "Nothing to undo." {
                    lines.push(format!("  {} {}", a_gray("│"), a_gray(output)));
                } else {
                    lines.push(format!("  {} {}", a_green("✓"), a_green(output)));
                }
            }
            "user_prompt_options" => {
                lines.push(format!("  {} {}", a_cyan("│"), a_cyan(output)));
            }
            _ => {
                lines.extend(Self::truncated_lines(output, DISPLAY_MAX_LINES));
            }
        }
        lines
    }

    fn format_diff_lines(diff: &str) -> Vec<String> {
        let src: Vec<&str> = diff.lines().collect();
        let total = src.len();
        let max = DISPLAY_MAX_LINES * 2;
        let mut out = Vec::new();

        let render = |line: &str| -> String {
            if line.starts_with("+++") || line.starts_with("---") {
                format!("    {}", a_bold(&a_white(line)))
            } else if line.starts_with("@@") {
                format!("    {}", a_cyan(line))
            } else if line.starts_with('+') {
                format!("    {}", a_green(line))
            } else if line.starts_with('-') {
                format!("    {}", a_red(line))
            } else {
                format!("    {}", a_gray(line))
            }
        };

        if total <= max {
            for l in &src {
                out.push(render(l));
            }
        } else {
            let head = max / 2;
            let tail = max - head;
            for l in &src[..head] {
                out.push(render(l));
            }
            out.push(format!(
                "    {}",
                a_gray(&format!("… ({} lines hidden) …", total - head - tail))
            ));
            for l in &src[total - tail..] {
                out.push(render(l));
            }
        }
        out
    }

    fn truncated_lines(output: &str, max: usize) -> Vec<String> {
        let src: Vec<&str> = output.lines().collect();
        let total = src.len();
        let mut out = Vec::new();
        if total <= max {
            for l in &src {
                out.push(format!("  {} {}", a_gray("│"), a_gray(l)));
            }
        } else {
            let head = max / 2;
            let tail = max - head;
            for l in &src[..head] {
                out.push(format!("  {} {}", a_gray("│"), a_gray(l)));
            }
            out.push(format!(
                "  {} {}",
                a_gray("│"),
                a_dim(&a_gray(&format!(
                    "  … ({} lines hidden) …",
                    total - head - tail
                )))
            ));
            for l in &src[total - tail..] {
                out.push(format!("  {} {}", a_gray("│"), a_gray(l)));
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Tool confirm picker (blocking, renders directly)
    // -----------------------------------------------------------------------

    fn run_tool_confirm_picker(
        &mut self,
        render_rx: &std_mpsc::Receiver<RenderCmd>,
        tool_call_id: &str,
        name: &str,
        args: &str,
    ) -> Option<ToolConfirmChoice> {
        // Show tool card in output buffer
        let args_val: serde_json::Value =
            serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
        let desc = format_tool_description(name, &args_val);

        self.push_line(&format!(
            "  {} {} {} {}",
            a_gray("┌─"),
            a_magenta("⚡"),
            a_magenta(name),
            a_gray("─")
        ));
        for l in &desc {
            self.push_line(&format!("  {}  {}", a_gray("│"), a_white(l)));
        }
        self.push_line(&format!("  {}", a_gray("└─")));

        let choices = ["Approve", "Deny", "Always approve for session"];
        let colors: [fn(&str) -> String; 3] = [a_green, a_red, a_yellow];
        let mut idx: usize = 0;

        // Add picker lines
        let picker_start = self.output_lines.len();
        for (i, label) in choices.iter().enumerate() {
            if i == idx {
                self.push_line(&format!("  {} {}", a_yellow("❯"), colors[i](label)));
            } else {
                self.push_line(&format!("    {}", a_gray(label)));
            }
        }
        self.push_line(&a_gray("  ↑↓ navigate  ⏎ select"));
        self.full_repaint();

        // Drain pending messages — if the resolution is already queued (replay),
        // skip the bell and return immediately.
        while let Ok(cmd) = render_rx.try_recv() {
            if let RenderCmd::ToolResolved {
                tool_call_id: resolved_id,
                approved,
            } = &cmd
            {
                if resolved_id == tool_call_id {
                    self.output_lines.truncate(picker_start);
                    let label = if *approved {
                        a_green("Approved (by another client)")
                    } else {
                        a_red("Denied (by another client)")
                    };
                    self.push_line(&format!("  {} {}", a_yellow("❯"), label));
                    self.push_line("");
                    return None; // resolved externally — no bell
                }
            }
            self.deferred_cmds.push(cmd);
        }

        // Play bell (only if prompt is genuinely waiting for user input)
        let _ = execute!(io::stdout(), Print("\x07"));
        let _ = io::stdout().flush();

        loop {
            // Check if another client resolved this tool confirmation
            if let Ok(cmd) = render_rx.try_recv() {
                if let RenderCmd::ToolResolved {
                    tool_call_id: resolved_id,
                    approved,
                } = &cmd
                {
                    if resolved_id == tool_call_id {
                        self.output_lines.truncate(picker_start);
                        let label = if *approved {
                            a_green("Approved (by another client)")
                        } else {
                            a_red("Denied (by another client)")
                        };
                        self.push_line(&format!("  {} {}", a_yellow("❯"), label));
                        self.push_line("");
                        return None; // resolved externally
                    }
                }
                // Queue other commands for later processing
                self.deferred_cmds.push(cmd);
            }

            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    match key.code {
                        KeyCode::Up => {
                            idx = if idx > 0 { idx - 1 } else { choices.len() - 1 };
                        }
                        KeyCode::Down => {
                            idx = if idx + 1 < choices.len() { idx + 1 } else { 0 };
                        }
                        KeyCode::Enter => {
                            // Update picker lines with final selection
                            self.output_lines.truncate(picker_start);
                            let chosen_label = choices[idx];
                            self.push_line(&format!(
                                "  {} {}",
                                a_yellow("❯"),
                                colors[idx](chosen_label)
                            ));
                            self.push_line("");
                            return Some(match idx {
                                0 => ToolConfirmChoice::Approve,
                                1 => ToolConfirmChoice::Deny,
                                _ => ToolConfirmChoice::Always,
                            });
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.output_lines.truncate(picker_start);
                            self.push_line("");
                            self.quit_requested = true;
                            return None;
                        }
                        _ => continue,
                    }
                    // Redraw picker
                    self.output_lines.truncate(picker_start);
                    for (i, label) in choices.iter().enumerate() {
                        if i == idx {
                            self.push_line(&format!("  {} {}", a_yellow("❯"), colors[i](label)));
                        } else {
                            self.push_line(&format!("    {}", a_gray(label)));
                        }
                    }
                    self.push_line(&a_gray("  ↑↓ navigate  ⏎ select"));
                    self.full_repaint();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // User prompt (blocking inline menu)
    // -----------------------------------------------------------------------

    fn run_inline_menu(
        &mut self,
        render_rx: &std_mpsc::Receiver<RenderCmd>,
        prompt_id: Option<&str>,
        question: &str,
        options: &[String],
        multi: bool,
    ) -> Option<Vec<usize>> {
        self.push_line(&format!("  {}", a_cyan(question)));
        self.push_line("");

        let mut idx: usize = 0;
        let mut selected: Vec<bool> = vec![false; options.len()];
        let menu_start = self.output_lines.len();

        let render_menu =
            |lines: &mut Vec<String>, opts: &[String], cur: usize, sel: &[bool], is_multi: bool| {
                for (i, opt) in opts.iter().enumerate() {
                    let focused = i == cur;
                    if is_multi {
                        let check = if sel[i] { "[x]" } else { "[ ]" };
                        if focused {
                            lines.push(format!(
                                "  {} {} {}",
                                a_yellow(check),
                                a_yellow("❯"),
                                a_white(opt)
                            ));
                        } else {
                            lines.push(format!("  {}   {}", a_gray(check), a_gray(opt)));
                        }
                    } else if focused {
                        lines.push(format!("  {} {}", a_yellow("❯"), a_white(opt)));
                    } else {
                        lines.push(format!("    {}", a_gray(opt)));
                    }
                }
                let hint = if is_multi {
                    "  ↑↓ navigate  ␣ toggle  ⏎ confirm"
                } else {
                    "  ↑↓ navigate  ⏎ select"
                };
                lines.push(a_gray(hint));
            };

        render_menu(&mut self.output_lines, options, idx, &selected, multi);
        self.scroll_offset = 0;
        self.full_repaint();

        // Drain pending messages — if the resolution is already queued (replay),
        // skip the bell and return immediately.
        while let Ok(cmd) = render_rx.try_recv() {
            if let Some(pid) = prompt_id {
                if let RenderCmd::PromptResolved {
                    prompt_id: resolved_id,
                } = &cmd
                {
                    if resolved_id == pid {
                        self.output_lines.truncate(menu_start);
                        self.push_line(&format!("  {}", a_gray("(resolved by another client)")));
                        self.push_line("");
                        return None; // resolved externally — no bell
                    }
                }
            }
            self.deferred_cmds.push(cmd);
        }

        // Play bell (only if prompt is genuinely waiting for user input)
        let _ = execute!(io::stdout(), Print("\x07"));
        let _ = io::stdout().flush();

        loop {
            // Check if another client already resolved this prompt
            if let Ok(cmd) = render_rx.try_recv() {
                if let Some(pid) = prompt_id {
                    if let RenderCmd::PromptResolved {
                        prompt_id: resolved_id,
                    } = &cmd
                    {
                        if resolved_id == pid {
                            self.output_lines.truncate(menu_start);
                            self.push_line(&format!(
                                "  {}",
                                a_gray("(resolved by another client)")
                            ));
                            self.push_line("");
                            return None;
                        }
                    }
                }
                // Queue other commands for later processing
                self.deferred_cmds.push(cmd);
            }

            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    match key.code {
                        KeyCode::Up => {
                            idx = if idx > 0 { idx - 1 } else { options.len() - 1 };
                        }
                        KeyCode::Down => {
                            idx = if idx + 1 < options.len() { idx + 1 } else { 0 };
                        }
                        KeyCode::Char(' ') if multi => {
                            selected[idx] = !selected[idx];
                        }
                        KeyCode::Enter => {
                            self.output_lines.truncate(menu_start);
                            if multi {
                                let sel: Vec<usize> = selected
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, &s)| s)
                                    .map(|(i, _)| i)
                                    .collect();
                                for (i, opt) in options.iter().enumerate() {
                                    if selected[i] {
                                        self.push_line(&format!(
                                            "  {} {}",
                                            a_green("[x]"),
                                            a_white(opt)
                                        ));
                                    }
                                }
                                self.push_line("");
                                return Some(sel);
                            } else {
                                self.push_line(&format!(
                                    "  {} {}",
                                    a_yellow("❯"),
                                    a_white(&options[idx])
                                ));
                                self.push_line("");
                                return Some(vec![idx]);
                            }
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.output_lines.truncate(menu_start);
                            self.push_line("");
                            self.quit_requested = true;
                            return None;
                        }
                        _ => continue,
                    }
                    self.output_lines.truncate(menu_start);
                    render_menu(&mut self.output_lines, options, idx, &selected, multi);
                    self.scroll_offset = 0;
                    self.full_repaint();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Handle render commands
    // -----------------------------------------------------------------------

    fn handle_render(&mut self, cmd: RenderCmd) {
        match cmd {
            RenderCmd::AssistantChunk(text) => {
                // Remove the "Thinking…" line if present (before any streaming check)
                if self.thinking_line_shown {
                    self.output_lines.pop();
                    self.thinking_line_shown = false;
                }
                if !self.streaming {
                    self.streaming = true;
                    self.streaming_buf.clear();
                }
                self.streaming_buf.push_str(&text);
                // Re-render streaming lines in output buffer
                self.flush_streaming_to_output();
                self.full_repaint();
            }
            RenderCmd::AssistantDone => {
                if self.streaming {
                    self.streaming = false;
                    self.flush_streaming_to_output();
                    self.streaming_buf.clear();
                    self.push_line("");
                }
                // LLM finished — dismiss any pending interrupt warning
                self.dismiss_interrupt_warning();
                self.full_repaint();
            }
            RenderCmd::Notice(text) => {
                for line in text.lines() {
                    self.push_line(&format!("{} {}", a_yellow("[notice]"), a_yellow(line)));
                }
                self.full_repaint();
            }
            RenderCmd::Error(text) => {
                for line in text.lines() {
                    self.push_line(&format!("{} {}", a_red("[error]"), a_red(line)));
                }
                self.full_repaint();
            }
            RenderCmd::ToolRequest { .. } => {
                unreachable!("ToolRequest should be intercepted before handle_render");
            }
            RenderCmd::ToolOutput {
                tool_name,
                tool_args,
                output,
            } => {
                let lines = self.render_tool_output_to_buf(&tool_name, &tool_args, &output);
                self.push_lines(lines);
                self.full_repaint();
            }
            RenderCmd::ProcessEvent(text) => {
                self.push_line(&format!("  {} {}", a_magenta("[proc]"), a_magenta(&text)));
                self.full_repaint();
            }
            RenderCmd::SessionInfo(name, cwd) => {
                self.session_name = name.clone();
                self.session_cwd = cwd.clone();
                self.push_line("");
                self.push_line(&format!(
                    "  {}",
                    a_cyan(&format!("Connected to session {name}"))
                ));
                self.push_line(&format!(
                    "  {}",
                    a_gray(&format!("Working directory: {cwd}"))
                ));
                self.push_line(&format!(
                    "  {}",
                    a_gray("(session persists even if this terminal closes)")
                ));
                self.push_line(&format!("  {}", a_gray("Type /help for commands")));
                self.push_line("");
                self.full_repaint();
            }
            RenderCmd::SlashCommands(commands) => {
                self.slash_commands = commands;
            }
            RenderCmd::SessionRenamed(name) => {
                self.session_name = name;
                self.full_repaint();
            }
            RenderCmd::ClientState { input_history } => {
                // Replace local history with server-side shared history
                self.history = input_history;
                self.history_idx = None;
                // auto_approved is handled in main.rs
            }
            RenderCmd::TaskPlan { .. } => {
                // Handled in the render loop via run_inline_menu
            }
            RenderCmd::TaskProgress {
                task_id,
                status,
                detail,
            } => {
                let icon = match status.as_str() {
                    "in_progress" => "→",
                    "completed" => "✓",
                    "failed" => "✗",
                    _ => "○",
                };
                let color_fn: fn(&str) -> String = match status.as_str() {
                    "in_progress" => a_yellow,
                    "completed" => a_green,
                    "failed" => a_red,
                    _ => a_gray,
                };
                let detail_str = detail.map(|d| format!(" — {d}")).unwrap_or_default();
                self.push_line(&format!(
                    "  {} {}",
                    color_fn(&format!("{icon} Task {task_id}")),
                    a_gray(&detail_str)
                ));
                self.full_repaint();
            }
            RenderCmd::SubagentUpdate {
                subagent_id,
                description,
                status,
                detail,
            } => {
                // Track active subagent count
                match status.as_str() {
                    "running" => {
                        self.active_subagents.insert(subagent_id);
                    }
                    "completed" | "failed" => {
                        self.active_subagents.remove(&subagent_id);
                    }
                    _ => {}
                }
                let icon = match status.as_str() {
                    "running" => "🔍",
                    "completed" => "✓",
                    "failed" => "✗",
                    _ => "·",
                };
                let detail_str = detail.map(|d| format!(" → {d}")).unwrap_or_default();
                let color_fn: fn(&str) -> String = match status.as_str() {
                    "running" => a_cyan,
                    "completed" => a_green,
                    "failed" => a_red,
                    _ => a_gray,
                };
                self.push_line(&format!(
                    "  {} {}{}",
                    icon,
                    color_fn(&description),
                    a_gray(&detail_str)
                ));
                self.full_repaint();
            }
            RenderCmd::PlanUpdate {
                _name: _,
                title,
                status,
                steps,
            } => {
                let status_color: fn(&str) -> String = match status.as_str() {
                    "completed" => a_green,
                    "failed" => a_red,
                    "in_progress" => a_yellow,
                    "partially_implemented" => a_cyan,
                    _ => a_gray,
                };
                self.push_line("");
                self.push_line(&format!(
                    "  {} {}",
                    a_cyan(&a_bold("Plan:")),
                    status_color(&format!("{title} [{status}]"))
                ));
                for (id, desc, st, detail) in &steps {
                    let icon = match st.as_str() {
                        "completed" => "✓",
                        "in_progress" => "→",
                        "failed" => "✗",
                        _ => "○",
                    };
                    let color_fn: fn(&str) -> String = match st.as_str() {
                        "completed" => a_green,
                        "in_progress" => a_yellow,
                        "failed" => a_red,
                        _ => a_gray,
                    };
                    let detail_str = detail
                        .as_ref()
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default();
                    self.push_line(&format!(
                        "    {} {}{}",
                        color_fn(icon),
                        color_fn(&format!("{id}: {desc}")),
                        a_gray(&detail_str)
                    ));
                }
                self.push_line("");
                self.full_repaint();
            }
            RenderCmd::UserInput { text } => {
                if self.awaiting_input_echo {
                    // This is our own echo — already rendered locally in submit()
                    self.awaiting_input_echo = false;
                } else {
                    // Another client submitted this prompt — display it
                    let prompt = if text.starts_with('/') {
                        "cmd-> "
                    } else if text.starts_with('!') {
                        "shell>"
                    } else {
                        "bear> "
                    };
                    self.push_line(&format!("  {}{}", a_bold(&a_white(prompt)), a_white(&text)));
                    self.full_repaint();
                }
            }
            RenderCmd::Thinking => {
                if !self.thinking_line_shown {
                    self.streaming = true;
                    self.streaming_buf.clear();
                    self.push_line(&format!("  {}", a_gray("⟳ Thinking…")));
                    self.thinking_line_shown = true;
                    self.full_repaint();
                }
            }
            RenderCmd::SuppressNextInputEcho => {
                self.awaiting_input_echo = true;
            }
            RenderCmd::UserPrompt { .. } => {}
            RenderCmd::ToolResolved { .. } => {}
            RenderCmd::PromptResolved { .. } => {}
            RenderCmd::Quit => {}
        }
    }

    /// Flush current streaming buffer into output lines.
    /// Removes previously written streaming lines and replaces them.
    /// Lines inside `<think>` blocks or starting with `THOUGHT:` are rendered
    /// in muted gray. Other content is rendered with markdown formatting.
    fn flush_streaming_to_output(&mut self) {
        // Remove any previous streaming lines (marked with a tag)
        while self
            .output_lines
            .last()
            .map(|l| l.starts_with("\x01STREAM\x01"))
            .unwrap_or(false)
        {
            self.output_lines.pop();
        }

        // First pass: separate think blocks from normal content so we can
        // render normal content through the markdown pipeline.
        let mut in_think = false;
        let mut in_code_block = false;
        let raw_lines: Vec<&str> = self.streaming_buf.lines().collect();

        for (i, line) in raw_lines.iter().enumerate() {
            let prefix = if i == 0 { "🐻 " } else { "   " };
            let trimmed = line.trim();

            // Track <think> blocks
            if trimmed.starts_with("<think>") || trimmed.starts_with("<think ") {
                in_think = true;
            }

            let is_thought = in_think
                || trimmed.starts_with("THOUGHT:")
                || trimmed.starts_with("Thought:")
                || trimmed.starts_with("thought:");

            if is_thought {
                self.output_lines
                    .push(format!("\x01STREAM\x01  {}{}", prefix, a_gray(line)));
            } else {
                // Render through markdown pipeline (one line at a time,
                // but tracking code-block state across lines)
                let md_lines = render_md_lines(line, &mut in_code_block);
                for (j, md_line) in md_lines.iter().enumerate() {
                    let p = if i == 0 && j == 0 { "🐻 " } else { "   " };
                    self.output_lines
                        .push(format!("\x01STREAM\x01  {}{}", p, md_line));
                }
            }

            if trimmed.contains("</think>") {
                in_think = false;
            }
        }
        self.scroll_offset = 0;
    }

    // -----------------------------------------------------------------------
    // Input editing
    // -----------------------------------------------------------------------

    fn insert_char(&mut self, c: char) {
        self.input_buf.insert(self.cursor_pos, c);
        self.cursor_pos += 1;
        self.history_idx = None;
    }

    fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            self.input_buf.remove(self.cursor_pos);
        }
    }

    fn delete(&mut self) {
        if self.cursor_pos < self.input_buf.len() {
            self.input_buf.remove(self.cursor_pos);
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor_pos < self.input_buf.len() {
            self.cursor_pos += 1;
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.saved_input = self.input_buf.clone();
                self.history_idx = Some(self.history.len() - 1);
            }
            Some(0) => return,
            Some(idx) => {
                self.history_idx = Some(idx - 1);
            }
        }
        if let Some(idx) = self.history_idx {
            self.input_buf = self.history[idx].clone();
            self.cursor_pos = self.input_buf.len();
        }
    }

    fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(idx) => {
                if idx + 1 >= self.history.len() {
                    self.history_idx = None;
                    self.input_buf = self.saved_input.clone();
                } else {
                    self.history_idx = Some(idx + 1);
                    self.input_buf = self.history[idx + 1].clone();
                }
                self.cursor_pos = self.input_buf.len();
            }
        }
    }

    fn submit(&mut self) -> String {
        let text = self.input_buf.clone();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }

        // Show submitted line in output
        let prompt = if text.starts_with('/') {
            "cmd-> "
        } else if text.starts_with('!') {
            "shell>"
        } else {
            "bear> "
        };
        self.push_line(&format!("  {}{}", a_bold(&a_white(prompt)), a_white(&text)));

        self.input_buf.clear();
        self.cursor_pos = 0;
        self.history_idx = None;
        self.dropdown_idx = None;
        text
    }
}

impl Drop for TermState {
    fn drop(&mut self) {
        self.cleanup();
    }
}

// ---------------------------------------------------------------------------
// Key mapping
// ---------------------------------------------------------------------------

enum KeyAction {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    HistoryPrev,
    HistoryNext,
    ScrollUp,
    ScrollDown,
    Tab,
    Escape,
    Submit,
    Quit,
    None,
}

fn map_key(key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Enter => KeyAction::Submit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyAction::Quit,
        KeyCode::Up => KeyAction::HistoryPrev,
        KeyCode::Down => KeyAction::HistoryNext,
        KeyCode::Left => KeyAction::Left,
        KeyCode::Right => KeyAction::Right,
        KeyCode::Backspace => KeyAction::Backspace,
        KeyCode::Delete => KeyAction::Delete,
        KeyCode::Home => KeyAction::Home,
        KeyCode::End => KeyAction::End,
        KeyCode::Tab => KeyAction::Tab,
        KeyCode::Esc => KeyAction::Escape,
        KeyCode::PageUp => KeyAction::ScrollUp,
        KeyCode::PageDown => KeyAction::ScrollDown,
        KeyCode::Char(c) => KeyAction::Char(c),
        _ => KeyAction::None,
    }
}
