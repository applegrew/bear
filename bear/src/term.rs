use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::io::{self, Write};
use std::sync::mpsc as std_mpsc;

// ---------------------------------------------------------------------------
// Messages the terminal thread can receive
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RenderCmd {
    Assistant(String),
    Notice(String),
    Error(String),
    ToolRequest(String, String),
    ToolOutput(String),
    ProcessEvent(String),
    SessionInfo(String, String),
    Quit,
}

// ---------------------------------------------------------------------------
// Events the terminal thread sends out
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum TermEvent {
    UserLine(String),
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
                    match map_key(key) {
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
}

const PROMPT: &str = "bear> ";
const PROMPT_CONFIRM: &str = "  Allow? [y/n/a(lways)] > ";

impl TermState {
    fn init() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            input_buf: String::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
        })
    }

    fn cleanup(&self) {
        let _ = terminal::disable_raw_mode();
    }

    fn draw_prompt(&self) {
        let mut out = io::stdout();
        let _ = execute!(
            out,
            Print("\r"),
            terminal::Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Cyan),
            Print(PROMPT),
            ResetColor,
            Print(&self.input_buf),
        );
        let back = self.input_buf.len() - self.cursor_pos;
        if back > 0 {
            let _ = execute!(out, cursor::MoveLeft(back as u16));
        }
        let _ = out.flush();
    }

    fn print_block(&self, prefix: &str, prefix_color: Color, body: &str) {
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

    fn handle_render(&self, cmd: RenderCmd) {
        match cmd {
            RenderCmd::Assistant(text) => {
                self.print_block("  ", Color::Green, &text);
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
            RenderCmd::ToolRequest(name, args) => {
                let mut out = io::stdout();
                let _ = execute!(
                    out,
                    Print("\r"),
                    terminal::Clear(ClearType::CurrentLine),
                    SetForegroundColor(Color::Magenta),
                    Print("  [tool] "),
                    ResetColor,
                    Print(&name),
                    Print("\r\n"),
                );
                for line in args.lines() {
                    let _ = execute!(
                        out,
                        SetForegroundColor(Color::DarkGrey),
                        Print("    "),
                        ResetColor,
                        Print(line),
                        Print("\r\n"),
                    );
                }
                let _ = execute!(
                    out,
                    SetForegroundColor(Color::Yellow),
                    Print(PROMPT_CONFIRM),
                    ResetColor,
                );
                let _ = out.flush();
            }
            RenderCmd::ToolOutput(text) => {
                self.print_block("  | ", Color::DarkGrey, &text);
                self.draw_prompt();
            }
            RenderCmd::ProcessEvent(text) => {
                self.print_block("  [proc] ", Color::Blue, &text);
                self.draw_prompt();
            }
            RenderCmd::SessionInfo(id, cwd) => {
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
        self.input_buf.clear();
        self.cursor_pos = 0;
        self.history_idx = None;

        let mut out = io::stdout();
        let _ = execute!(
            out,
            Print("\r"),
            terminal::Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::White),
            Print(PROMPT),
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
        KeyCode::Char(c) => KeyAction::Char(c),
        _ => KeyAction::None,
    }
}
