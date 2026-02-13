use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
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
        base_command: String,
        name: String,
        args: String,
    },
    ToolOutput { tool_name: String, tool_args: serde_json::Value, output: String },
    ProcessEvent(String),
    SessionInfo(String, String),
    UserPrompt {
        prompt_id: String,
        question: String,
        options: Vec<String>,
        multi: bool,
    },
    Thinking,
    Quit,
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
        base_command: String,
        choice: ToolConfirmChoice,
    },
    UserPromptResult { prompt_id: String, selected: Vec<usize> },
    Interrupt,
    Quit,
}

// ---------------------------------------------------------------------------
// Terminal thread: owns raw mode, input buffer, history, and rendering
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

        state.draw_prompt();

        loop {
            // Drain all pending render commands (non-blocking)
            loop {
                match render_rx.try_recv() {
                    Ok(cmd) => {
                        if matches!(cmd, RenderCmd::Quit) {
                            state.cleanup();
                            return;
                        }
                        // Intercept ToolRequest: run inline picker, send result
                        if let RenderCmd::ToolRequest { tool_call_id, base_command, name, args } = cmd {
                            let choice = state.run_tool_confirm_picker(&name, &args);
                            let _ = rt.block_on(event_tx.send(
                                TermEvent::ToolConfirmResult { tool_call_id, base_command, choice },
                            ));
                            state.draw_prompt();
                            continue;
                        }
                        // Intercept UserPrompt: run inline menu, send result
                        if let RenderCmd::UserPrompt { prompt_id, question, options, multi } = cmd {
                            let selected = state.run_inline_menu(&question, &options, multi);
                            let _ = rt.block_on(event_tx.send(
                                TermEvent::UserPromptResult { prompt_id, selected },
                            ));
                            state.draw_prompt();
                            continue;
                        }
                        state.handle_render(cmd);
                    }
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => {
                        state.cleanup();
                        return;
                    }
                }
            }

            // Poll for keyboard input (50ms timeout)
            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    let action = map_key(key);

                    // When the dropdown is visible, intercept navigation keys
                    if state.dropdown_active() {
                        match action {
                            KeyAction::HistoryPrev => {
                                // Up arrow → move selection up
                                state.dropdown_up();
                                state.draw_prompt();
                                continue;
                            }
                            KeyAction::HistoryNext => {
                                // Down arrow → move selection down
                                state.dropdown_down();
                                state.draw_prompt();
                                continue;
                            }
                            KeyAction::Tab => {
                                // Tab → accept selected item
                                state.accept_dropdown();
                                state.draw_prompt();
                                continue;
                            }
                            KeyAction::Submit => {
                                // Enter → accept selected item (if one is highlighted)
                                if state.dropdown_idx.is_some() {
                                    state.accept_dropdown();
                                    state.draw_prompt();
                                    continue;
                                }
                                // Otherwise fall through to normal submit
                            }
                            KeyAction::Escape => {
                                // Esc → close dropdown, clear input
                                state.input_buf.clear();
                                state.cursor_pos = 0;
                                state.dropdown_idx = None;
                                state.draw_prompt();
                                continue;
                            }
                            _ => {
                                // Any other key: reset selection so typing
                                // re-filters from scratch
                                state.dropdown_idx = None;
                            }
                        }
                    }

                    match action {
                        KeyAction::Char(c) => {
                            state.insert_char(c);
                            state.draw_prompt();
                        }
                        KeyAction::Backspace => {
                            state.backspace();
                            state.draw_prompt();
                        }
                        KeyAction::Delete => {
                            state.delete();
                            state.draw_prompt();
                        }
                        KeyAction::Left => {
                            state.cursor_left();
                            state.draw_prompt();
                        }
                        KeyAction::Right => {
                            state.cursor_right();
                            state.draw_prompt();
                        }
                        KeyAction::Home => {
                            state.cursor_pos = 0;
                            state.draw_prompt();
                        }
                        KeyAction::End => {
                            state.cursor_pos = state.input_buf.len();
                            state.draw_prompt();
                        }
                        KeyAction::HistoryPrev => {
                            state.history_prev();
                            state.draw_prompt();
                        }
                        KeyAction::HistoryNext => {
                            state.history_next();
                            state.draw_prompt();
                        }
                        KeyAction::Tab => {
                            // Tab outside dropdown — no-op
                        }
                        KeyAction::Escape => {
                            // Esc outside dropdown — no-op
                        }
                        KeyAction::Submit => {
                            let line = state.submit();
                            let _ = rt.block_on(event_tx.send(TermEvent::UserLine(line)));
                            state.draw_prompt();
                        }
                        KeyAction::Interrupt => {
                            let _ = rt.block_on(event_tx.send(TermEvent::Interrupt));
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
        }
    })
}

// ---------------------------------------------------------------------------
// Internal terminal state
// ---------------------------------------------------------------------------

struct TermState {
    input_buf: String,
    cursor_pos: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    saved_input: String,
    streaming: bool,
    /// Number of dropdown lines currently rendered below the prompt.
    dropdown_lines: usize,
    /// Currently selected dropdown item index (None = no selection).
    dropdown_idx: Option<usize>,
}

const PROMPT: &str = "bear> ";
const PROMPT_CMD: &str = "cmd-> ";
/// All available slash commands with short descriptions.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/ps", "List background processes"),
    ("/kill", "Kill a background process"),
    ("/send", "Send stdin to a process"),
    ("/session name", "Name the current session"),
    ("/allowed", "Show auto-approved commands"),
    ("/exit", "Disconnect, keep session alive"),
    ("/end", "End session, pick another"),
    ("/help", "Show help"),
];

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
            let preview = if find.len() > 60 { &find[..60] } else { find };
            vec![
                format!("Editing: {path}"),
                format!("Find: {preview}…"),
            ]
        }
        "patch_file" => {
            let path = args["path"].as_str().unwrap_or("(unknown)");
            vec![format!("Patching: {path}")]
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
            // Fallback: show key=value pairs
            if let Some(obj) = args.as_object() {
                obj.iter().map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => {
                            if s.len() > 60 { format!("{}…", &s[..60]) } else { s.clone() }
                        }
                        other => {
                            let s = other.to_string();
                            if s.len() > 60 { format!("{}…", &s[..60]) } else { s }
                        }
                    };
                    format!("{k}: {val}")
                }).collect()
            } else {
                vec![args.to_string()]
            }
        }
    }
}

/// Return up to 3 slash commands matching the current input prefix.
fn matching_slash_commands(input: &str) -> Vec<(&'static str, &'static str)> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    // Match against the typed text (which may be just "/")
    let typed = input.split_whitespace().next().unwrap_or(input);
    SLASH_COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd.starts_with(typed) || typed.starts_with(cmd))
        .take(3)
        .copied()
        .collect()
}

impl TermState {
    fn init() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            input_buf: String::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            streaming: false,
            dropdown_lines: 0,
            dropdown_idx: None,
        })
    }

    fn cleanup(&self) {
        let _ = terminal::disable_raw_mode();
    }

    fn draw_prompt(&mut self) {
        let mut out = io::stdout();

        // Clear any previous dropdown lines
        if self.dropdown_lines > 0 {
            // Save cursor, move down to clear dropdown, then restore
            let _ = execute!(out, cursor::SavePosition);
            for _ in 0..self.dropdown_lines {
                let _ = execute!(out, Print("\r\n"), terminal::Clear(ClearType::CurrentLine));
            }
            let _ = execute!(out, cursor::RestorePosition);
            self.dropdown_lines = 0;
        }

        let is_slash = self.input_buf.starts_with('/');
        let (prompt, prompt_color) = if is_slash {
            (PROMPT_CMD, Color::Yellow)
        } else {
            (PROMPT, Color::Cyan)
        };

        let _ = execute!(
            out,
            Print("\r"),
            terminal::Clear(ClearType::CurrentLine),
            SetForegroundColor(prompt_color),
            Print(prompt),
            ResetColor,
            Print(&self.input_buf),
        );

        // Render dropdown for slash commands
        let matches = matching_slash_commands(&self.input_buf);
        if is_slash && !matches.is_empty() {
            // Clamp dropdown_idx to valid range
            if let Some(idx) = self.dropdown_idx {
                if idx >= matches.len() {
                    self.dropdown_idx = Some(matches.len() - 1);
                }
            }
            let _ = execute!(out, cursor::SavePosition);
            for (i, (cmd, desc)) in matches.iter().enumerate() {
                let selected = self.dropdown_idx == Some(i);
                let _ = execute!(out, Print("\r\n"), terminal::Clear(ClearType::CurrentLine));
                if selected {
                    let _ = execute!(
                        out,
                        SetForegroundColor(Color::Yellow),
                        Print("    ❯ "),
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
                        Print("      "),
                        SetForegroundColor(Color::Yellow),
                        Print(cmd),
                        SetForegroundColor(Color::DarkGrey),
                        Print("  "),
                        Print(desc),
                        ResetColor,
                    );
                }
            }
            self.dropdown_lines = matches.len();
            let _ = execute!(out, cursor::RestorePosition);
        } else {
            self.dropdown_idx = None;
        }

        // Position cursor correctly within the input
        let back = self.input_buf.len() - self.cursor_pos;
        if back > 0 {
            let _ = execute!(out, cursor::MoveLeft(back as u16));
        }
        let _ = out.flush();
    }

    /// Returns true if the slash-command dropdown is currently visible.
    fn dropdown_active(&self) -> bool {
        self.dropdown_lines > 0 && self.input_buf.starts_with('/')
    }

    fn clear_dropdown(&mut self) {
        if self.dropdown_lines > 0 {
            let mut out = io::stdout();
            let _ = execute!(out, cursor::SavePosition);
            for _ in 0..self.dropdown_lines {
                let _ = execute!(out, Print("\r\n"), terminal::Clear(ClearType::CurrentLine));
            }
            let _ = execute!(out, cursor::RestorePosition);
            let _ = out.flush();
            self.dropdown_lines = 0;
        }
    }

    /// Move dropdown selection up.
    fn dropdown_up(&mut self) {
        let matches = matching_slash_commands(&self.input_buf);
        if matches.is_empty() { return; }
        self.dropdown_idx = Some(match self.dropdown_idx {
            None | Some(0) => matches.len() - 1,
            Some(i) => i - 1,
        });
    }

    /// Move dropdown selection down.
    fn dropdown_down(&mut self) {
        let matches = matching_slash_commands(&self.input_buf);
        if matches.is_empty() { return; }
        self.dropdown_idx = Some(match self.dropdown_idx {
            None => 0,
            Some(i) if i + 1 >= matches.len() => 0,
            Some(i) => i + 1,
        });
    }

    /// Accept the currently selected dropdown item: fill input_buf with
    /// the command text and a trailing space, then close the dropdown.
    fn accept_dropdown(&mut self) {
        let matches = matching_slash_commands(&self.input_buf);
        let idx = self.dropdown_idx.unwrap_or(0);
        if let Some((cmd, _)) = matches.get(idx) {
            self.input_buf = format!("{} ", cmd);
            self.cursor_pos = self.input_buf.len();
        }
        self.dropdown_idx = None;
    }

    fn print_block(&mut self, prefix: &str, prefix_color: Color, body: &str) {
        self.clear_dropdown();
        let mut out = io::stdout();
        let _ = execute!(out, Print("\r"), terminal::Clear(ClearType::CurrentLine));
        for line in body.lines() {
            let _ = execute!(
                out,
                SetForegroundColor(prefix_color),
                Print(prefix),
                ResetColor,
                Print(line),
                Print("\r\n"),
            );
        }
        let _ = out.flush();
    }

    /// Max lines to display for tool output before truncating.
    const DISPLAY_MAX_LINES: usize = 20;

    /// Render tool output with tool-specific formatting.
    fn render_tool_output(&mut self, tool_name: &str, tool_args: &serde_json::Value, output: &str) {
        match tool_name {
            "read_file" => {
                let path = tool_args["path"].as_str().unwrap_or("?");
                let line_count = output.lines().count();
                if output.starts_with("Error") {
                    self.print_block("  ✗ ", Color::Red, output);
                } else {
                    self.print_block(
                        "  ✓ ",
                        Color::Green,
                        &format!("Read {} ({} lines)", path, line_count),
                    );
                }
            }
            "write_file" | "edit_file" | "patch_file" => {
                let is_err = output.starts_with("Error") || output.starts_with("Patch failed");
                // Split status line from diff (separated by blank line)
                let (status, diff) = if let Some(pos) = output.find("\n\n") {
                    (&output[..pos], Some(output[pos + 2..].trim_end()))
                } else {
                    (output, None)
                };
                let icon = if is_err { ("  ✗ ", Color::Red) } else { ("  ✓ ", Color::Green) };
                self.print_block(icon.0, icon.1, status);
                if let Some(diff_text) = diff {
                    self.print_diff(diff_text);
                }
            }
            "run_command" => {
                self.print_truncated_output(output);
            }
            "list_files" => {
                let count = output.lines().count();
                self.print_block("  ✓ ", Color::Green, &format!("{count} entries"));
                self.print_truncated_output(output);
            }
            "search_text" => {
                let count = output.lines().filter(|l| !l.starts_with('[') && !l.is_empty()).count();
                if output == "No matches found." {
                    self.print_block("  │ ", Color::DarkGrey, output);
                } else {
                    self.print_block("  ✓ ", Color::Green, &format!("{count} matches"));
                    self.print_truncated_output(output);
                }
            }
            "undo" => {
                let icon = if output.starts_with("Error") || output == "Nothing to undo." {
                    ("  │ ", Color::DarkGrey)
                } else {
                    ("  ✓ ", Color::Green)
                };
                self.print_block(icon.0, icon.1, output);
            }
            "user_prompt_options" => {
                self.print_block("  │ ", Color::Cyan, output);
            }
            _ => {
                // Unknown tool — show truncated output
                self.print_truncated_output(output);
            }
        }
    }

    /// Print a unified diff with syntax-colored lines.
    fn print_diff(&mut self, diff: &str) {
        let mut out = io::stdout();
        let lines: Vec<&str> = diff.lines().collect();
        let total = lines.len();
        let max = Self::DISPLAY_MAX_LINES * 2; // allow more lines for diffs
        let show: &[&str] = if total <= max {
            &lines
        } else {
            // Show head + truncation notice + tail
            let head = max / 2;
            let tail = max - head;
            let _ = execute!(out, Print("\r"));
            for line in &lines[..head] {
                Self::print_diff_line(&mut out, line);
            }
            let _ = execute!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(format!("    … ({} lines hidden) …\r\n", total - head - tail)),
                ResetColor,
            );
            for line in &lines[total - tail..] {
                Self::print_diff_line(&mut out, line);
            }
            let _ = out.flush();
            return;
        };
        let _ = execute!(out, Print("\r"));
        for line in show {
            Self::print_diff_line(&mut out, line);
        }
        let _ = out.flush();
    }

    fn print_diff_line(out: &mut io::Stdout, line: &str) {
        if line.starts_with("+++") || line.starts_with("---") {
            let _ = execute!(
                out,
                SetAttribute(Attribute::Bold),
                SetForegroundColor(Color::White),
                Print(format!("    {line}\r\n")),
                ResetColor,
                SetAttribute(Attribute::Reset),
            );
        } else if line.starts_with("@@") {
            let _ = execute!(
                out,
                SetForegroundColor(Color::Cyan),
                Print(format!("    {line}\r\n")),
                ResetColor,
            );
        } else if line.starts_with('+') {
            let _ = execute!(
                out,
                SetForegroundColor(Color::Green),
                Print(format!("    {line}\r\n")),
                ResetColor,
            );
        } else if line.starts_with('-') {
            let _ = execute!(
                out,
                SetForegroundColor(Color::Red),
                Print(format!("    {line}\r\n")),
                ResetColor,
            );
        } else {
            let _ = execute!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(format!("    {line}\r\n")),
                ResetColor,
            );
        }
    }

    /// Print output with a line cap, showing head + tail with a truncation notice.
    fn print_truncated_output(&mut self, output: &str) {
        let lines: Vec<&str> = output.lines().collect();
        let total = lines.len();
        if total <= Self::DISPLAY_MAX_LINES {
            self.print_block("  │ ", Color::DarkGrey, output);
            return;
        }
        let head = Self::DISPLAY_MAX_LINES / 2;
        let tail = Self::DISPLAY_MAX_LINES - head;
        let mut display = String::new();
        for line in &lines[..head] {
            display.push_str(line);
            display.push('\n');
        }
        display.push_str(&format!(
            "  … ({} lines hidden) …\n",
            total - head - tail
        ));
        for line in &lines[total - tail..] {
            display.push_str(line);
            display.push('\n');
        }
        self.print_block("  │ ", Color::DarkGrey, display.trim_end());
    }

    /// Run a blocking tool-confirmation picker. Shows tool info then a 3-option
    /// menu: Approve / Deny / Always approve.
    fn run_tool_confirm_picker(&self, name: &str, args: &str) -> ToolConfirmChoice {
        let mut out = io::stdout();

        // Parse args JSON for formatting
        let args_val: serde_json::Value = serde_json::from_str(args)
            .unwrap_or(serde_json::Value::Null);
        let desc_lines = format_tool_description(name, &args_val);

        // Draw card
        let _ = execute!(out, Print("\r"), terminal::Clear(ClearType::CurrentLine));
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("  ┌─ "),
            SetForegroundColor(Color::Magenta),
            Print("⚡ "),
            Print(name),
            SetForegroundColor(Color::DarkGrey),
            Print(" ─"),
            ResetColor,
            Print("\r\n"),
        );
        for line in &desc_lines {
            let _ = execute!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print("  │  "),
                SetForegroundColor(Color::White),
                Print(line),
                ResetColor,
                Print("\r\n"),
            );
        }
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("  └─"),
            ResetColor,
            Print("\r\n"),
        );
        let _ = out.flush();

        let choices = ["Approve", "Deny", "Always approve for session"];
        let choice_colors = [Color::Green, Color::Red, Color::Yellow];
        let mut cursor_idx: usize = 0;

        let draw = |out: &mut io::Stdout, cur: usize, redraw: bool| {
            if redraw {
                // Move up to overwrite previous render (choices + hint = choices.len() + 1 lines)
                for _ in 0..choices.len() + 1 {
                    let _ = execute!(out, cursor::MoveUp(1));
                }
                let _ = execute!(out, Print("\r"), terminal::Clear(ClearType::FromCursorDown));
            }
            for (i, label) in choices.iter().enumerate() {
                let focused = i == cur;
                if focused {
                    let _ = execute!(
                        out,
                        SetForegroundColor(Color::Yellow),
                        Print("  ❯ "),
                        SetForegroundColor(choice_colors[i]),
                        Print(label),
                        ResetColor,
                        Print("\r\n"),
                    );
                } else {
                    let _ = execute!(
                        out,
                        SetForegroundColor(Color::DarkGrey),
                        Print("    "),
                        Print(label),
                        ResetColor,
                        Print("\r\n"),
                    );
                }
            }
            let _ = execute!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print("  ↑↓ navigate  ⏎ select"),
                ResetColor,
            );
            let _ = out.flush();
        };

        draw(&mut out, cursor_idx, false);

        loop {
            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    match key.code {
                        KeyCode::Up => {
                            cursor_idx = if cursor_idx > 0 { cursor_idx - 1 } else { choices.len() - 1 };
                            draw(&mut out, cursor_idx, true);
                        }
                        KeyCode::Down => {
                            cursor_idx = if cursor_idx + 1 < choices.len() { cursor_idx + 1 } else { 0 };
                            draw(&mut out, cursor_idx, true);
                        }
                        KeyCode::Enter => {
                            // Clear the hint line
                            let _ = execute!(out, Print("\r"), terminal::Clear(ClearType::CurrentLine), Print("\r\n"));
                            let _ = out.flush();
                            return match cursor_idx {
                                0 => ToolConfirmChoice::Approve,
                                1 => ToolConfirmChoice::Deny,
                                _ => ToolConfirmChoice::Always,
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Run an inline interactive menu (blocking). Returns selected indices.
    fn run_inline_menu(&self, question: &str, options: &[String], multi: bool) -> Vec<usize> {
        let mut out = io::stdout();
        let mut cursor_idx: usize = 0;
        let mut selected: Vec<bool> = vec![false; options.len()];

        // Print question
        let _ = execute!(
            out,
            Print("\r"),
            terminal::Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Cyan),
            Print("  "),
            Print(question),
            ResetColor,
            Print("\r\n"),
        );
        let _ = out.flush();

        let draw = |out: &mut io::Stdout, opts: &[String], cur: usize, sel: &[bool], is_multi: bool, redraw: bool| {
            if redraw {
                // Move up to overwrite previous render
                for _ in 0..opts.len() + 1 {
                    let _ = execute!(out, cursor::MoveUp(1));
                }
                let _ = execute!(out, Print("\r"), terminal::Clear(ClearType::FromCursorDown));
            }
            for (i, opt) in opts.iter().enumerate() {
                let focused = i == cur;
                if is_multi {
                    let check = if sel[i] { "[x]" } else { "[ ]" };
                    if focused {
                        let _ = execute!(
                            out,
                            SetForegroundColor(Color::Yellow),
                            Print("  "),
                            Print(check),
                            Print(" "),
                            SetForegroundColor(Color::White),
                            Print(opt),
                            ResetColor,
                            Print("\r\n"),
                        );
                    } else {
                        let _ = execute!(
                            out,
                            SetForegroundColor(Color::DarkGrey),
                            Print("  "),
                            Print(check),
                            Print(" "),
                            Print(opt),
                            ResetColor,
                            Print("\r\n"),
                        );
                    }
                } else if focused {
                    let _ = execute!(
                        out,
                        SetForegroundColor(Color::Yellow),
                        Print("  > "),
                        SetForegroundColor(Color::White),
                        Print(opt),
                        ResetColor,
                        Print("\r\n"),
                    );
                } else {
                    let _ = execute!(
                        out,
                        SetForegroundColor(Color::DarkGrey),
                        Print("    "),
                        Print(opt),
                        ResetColor,
                        Print("\r\n"),
                    );
                }
            }
            let hint = if is_multi { "  ↑↓ navigate  ␣ toggle  ⏎ confirm" } else { "  ↑↓ navigate  ⏎ select" };
            let _ = execute!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(hint),
                ResetColor,
            );
            let _ = out.flush();
        };

        draw(&mut out, options, cursor_idx, &selected, multi, false);

        loop {
            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    match key.code {
                        KeyCode::Up => {
                            cursor_idx = if cursor_idx > 0 { cursor_idx - 1 } else { options.len() - 1 };
                            draw(&mut out, options, cursor_idx, &selected, multi, true);
                        }
                        KeyCode::Down => {
                            cursor_idx = if cursor_idx + 1 < options.len() { cursor_idx + 1 } else { 0 };
                            draw(&mut out, options, cursor_idx, &selected, multi, true);
                        }
                        KeyCode::Char(' ') if multi => {
                            selected[cursor_idx] = !selected[cursor_idx];
                            draw(&mut out, options, cursor_idx, &selected, multi, true);
                        }
                        KeyCode::Enter => {
                            // Clear the hint line
                            let _ = execute!(out, Print("\r"), terminal::Clear(ClearType::CurrentLine), Print("\r\n"));
                            let _ = out.flush();
                            if multi {
                                return selected.iter().enumerate()
                                    .filter(|(_, &s)| s)
                                    .map(|(i, _)| i)
                                    .collect();
                            } else {
                                return vec![cursor_idx];
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn handle_render(&mut self, cmd: RenderCmd) {
        match cmd {
            RenderCmd::AssistantChunk(text) => {
                if !self.streaming {
                    // First chunk — clear prompt line and start green output
                    self.streaming = true;
                    self.clear_dropdown();
                    let mut out = io::stdout();
                    let _ = execute!(
                        out,
                        Print("\r"),
                        terminal::Clear(ClearType::CurrentLine),
                        SetForegroundColor(Color::Green),
                        Print("  "),
                    );
                    let _ = out.flush();
                }
                let mut out = io::stdout();
                // Print chunk, replacing newlines with \r\n + indent
                let formatted = text.replace('\n', "\r\n  ");
                let _ = execute!(
                    out,
                    SetForegroundColor(Color::Green),
                    Print(&formatted),
                );
                let _ = out.flush();
            }
            RenderCmd::AssistantDone => {
                if self.streaming {
                    self.streaming = false;
                    let mut out = io::stdout();
                    let _ = execute!(out, ResetColor, Print("\r\n"));
                    let _ = out.flush();
                }
                self.draw_prompt();
            }
            RenderCmd::Notice(text) => {
                self.print_block("[notice] ", Color::Yellow, &text);
                self.draw_prompt();
            }
            RenderCmd::Error(text) => {
                self.print_block("[error] ", Color::Red, &text);
                self.draw_prompt();
            }
            RenderCmd::ToolRequest { .. } => {
                // Handled by the blocking picker in the render loop; should not reach here.
                unreachable!("ToolRequest should be intercepted before handle_render");
            }
            RenderCmd::ToolOutput { tool_name, tool_args, output } => {
                self.render_tool_output(&tool_name, &tool_args, &output);
                self.draw_prompt();
            }
            RenderCmd::ProcessEvent(text) => {
                self.print_block("  [proc] ", Color::Blue, &text);
                self.draw_prompt();
            }
            RenderCmd::SessionInfo(id, cwd) => {
                self.clear_dropdown();
                let mut out = io::stdout();
                let _ = execute!(
                    out,
                    Print("\r\n"),
                    SetForegroundColor(Color::Cyan),
                    Print("  Connected to session "),
                    Print(&id),
                    Print("\r\n"),
                    Print("  Working directory: "),
                    Print(&cwd),
                    Print("\r\n"),
                    Print("  (session persists even if this terminal closes)\r\n"),
                    Print("  Type /help for commands\r\n"),
                    ResetColor,
                    Print("\r\n"),
                );
                let _ = out.flush();
                self.draw_prompt();
            }
            RenderCmd::Thinking => {
                self.clear_dropdown();
                let mut out = io::stdout();
                let _ = execute!(
                    out,
                    Print("\r"),
                    terminal::Clear(ClearType::CurrentLine),
                    SetForegroundColor(Color::DarkGrey),
                    Print("  ⟳ Thinking…"),
                    ResetColor,
                    Print("\r\n"),
                );
                let _ = out.flush();
            }
            RenderCmd::UserPrompt { .. } => {
                // Handled in the main loop before handle_render is called
            }
            RenderCmd::Quit => {}
        }
    }

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

        // Clear any dropdown lines before printing the submitted line
        let mut out = io::stdout();
        if self.dropdown_lines > 0 {
            let _ = execute!(out, cursor::SavePosition);
            for _ in 0..self.dropdown_lines {
                let _ = execute!(out, Print("\r\n"), terminal::Clear(ClearType::CurrentLine));
            }
            let _ = execute!(out, cursor::RestorePosition);
            self.dropdown_lines = 0;
        }

        // Show which prompt was active when submitted
        let prompt = if text.starts_with('/') { PROMPT_CMD } else { PROMPT };

        self.input_buf.clear();
        self.cursor_pos = 0;
        self.history_idx = None;
        self.dropdown_idx = None;

        let _ = execute!(
            out,
            Print("\r"),
            terminal::Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::White),
            Print(prompt),
            Print(&text),
            Print("\r\n"),
            ResetColor,
        );
        let _ = out.flush();
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
    Tab,
    Escape,
    Submit,
    Interrupt,
    Quit,
    None,
}

fn map_key(key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Enter => KeyAction::Submit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyAction::Interrupt,
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyAction::Quit,
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
        KeyCode::Char(c) => KeyAction::Char(c),
        _ => KeyAction::None,
    }
}
