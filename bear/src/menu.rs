use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::io::{self, Write};

pub struct MenuItem {
    pub label: String,
    pub description: String,
}

pub enum MenuMode {
    Single,
    #[allow(dead_code)]
    Multi,
}

#[allow(dead_code)]
pub enum MenuResult {
    Single(usize),
    Multi(Vec<usize>),
    Cancelled,
}

pub fn interactive_menu(
    title: &str,
    items: &[MenuItem],
    mode: MenuMode,
) -> MenuResult {
    if items.is_empty() {
        return MenuResult::Cancelled;
    }

    let mut stdout = io::stdout();
    let _ = terminal::enable_raw_mode();

    let mut cursor_idx: usize = 0;
    let mut selected: Vec<bool> = vec![false; items.len()];
    let is_multi = matches!(mode, MenuMode::Multi);

    draw_menu(&mut stdout, title, items, cursor_idx, &selected, is_multi);

    let result = loop {
        if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match key.code {
                    KeyCode::Up => {
                        if cursor_idx > 0 {
                            cursor_idx -= 1;
                        } else {
                            cursor_idx = items.len() - 1;
                        }
                        draw_menu(&mut stdout, title, items, cursor_idx, &selected, is_multi);
                    }
                    KeyCode::Down => {
                        if cursor_idx + 1 < items.len() {
                            cursor_idx += 1;
                        } else {
                            cursor_idx = 0;
                        }
                        draw_menu(&mut stdout, title, items, cursor_idx, &selected, is_multi);
                    }
                    KeyCode::Char(' ') if is_multi => {
                        selected[cursor_idx] = !selected[cursor_idx];
                        draw_menu(&mut stdout, title, items, cursor_idx, &selected, is_multi);
                    }
                    KeyCode::Enter => {
                        if is_multi {
                            let indices: Vec<usize> = selected
                                .iter()
                                .enumerate()
                                .filter(|(_, &s)| s)
                                .map(|(i, _)| i)
                                .collect();
                            break MenuResult::Multi(indices);
                        } else {
                            break MenuResult::Single(cursor_idx);
                        }
                    }
                    KeyCode::Char('q') | KeyCode::Esc => {
                        break MenuResult::Cancelled;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break MenuResult::Cancelled;
                    }
                    _ => {}
                }
            }
        }
    };

    let _ = terminal::disable_raw_mode();

    // Move cursor below the menu
    let _ = execute!(stdout, Print("\r\n"));
    let _ = stdout.flush();

    result
}

fn draw_menu(
    stdout: &mut io::Stdout,
    title: &str,
    items: &[MenuItem],
    cursor_idx: usize,
    selected: &[bool],
    is_multi: bool,
) {
    // Move to start: go up by (items.len() + 2) lines for title + hint + items
    // On first draw this overshoots but cursor::MoveUp(0) is a no-op issue,
    // so we just clear from cursor down.
    let total_lines = items.len() + 2; // title + items + hint
    for _ in 0..total_lines {
        let _ = execute!(stdout, cursor::MoveUp(1));
    }
    let _ = execute!(
        stdout,
        Print("\r"),
        terminal::Clear(ClearType::FromCursorDown),
    );

    // Title
    let _ = execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(title),
        ResetColor,
        Print("\r\n"),
    );

    // Items
    for (i, item) in items.iter().enumerate() {
        let is_focused = i == cursor_idx;
        let is_selected = selected[i];

        let _ = execute!(stdout, Print("  "));

        if is_multi {
            let check = if is_selected { "[x]" } else { "[ ]" };
            if is_focused {
                let _ = execute!(
                    stdout,
                    SetForegroundColor(Color::Yellow),
                    Print(check),
                    Print(" "),
                    ResetColor,
                );
            } else {
                let _ = execute!(stdout, Print(check), Print(" "));
            }
        } else if is_focused {
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::Yellow),
                Print("> "),
                ResetColor,
            );
        } else {
            let _ = execute!(stdout, Print("  "));
        }

        if is_focused {
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::White),
                Print(&item.label),
                ResetColor,
            );
        } else {
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print(&item.label),
                ResetColor,
            );
        }

        if !item.description.is_empty() {
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print("  "),
                Print(&item.description),
                ResetColor,
            );
        }

        let _ = execute!(stdout, Print("\r\n"));
    }

    // Hint line
    if is_multi {
        let _ = execute!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("  ↑↓ navigate  ␣ toggle  ⏎ confirm  q cancel"),
            ResetColor,
        );
    } else {
        let _ = execute!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("  ↑↓ navigate  ⏎ select  q cancel"),
            ResetColor,
        );
    }

    let _ = stdout.flush();
}
