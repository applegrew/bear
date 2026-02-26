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

pub fn interactive_menu(title: &str, items: &[MenuItem], mode: MenuMode) -> MenuResult {
    if items.is_empty() {
        return MenuResult::Cancelled;
    }

    let mut stdout = io::stdout();
    let _ = terminal::enable_raw_mode();

    let mut cursor_idx: usize = 0;
    let mut selected: Vec<bool> = vec![false; items.len()];
    let is_multi = matches!(mode, MenuMode::Multi);

    draw_menu(
        &mut stdout,
        title,
        items,
        cursor_idx,
        &selected,
        is_multi,
        true,
    );

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
                        draw_menu(
                            &mut stdout,
                            title,
                            items,
                            cursor_idx,
                            &selected,
                            is_multi,
                            false,
                        );
                    }
                    KeyCode::Down => {
                        if cursor_idx + 1 < items.len() {
                            cursor_idx += 1;
                        } else {
                            cursor_idx = 0;
                        }
                        draw_menu(
                            &mut stdout,
                            title,
                            items,
                            cursor_idx,
                            &selected,
                            is_multi,
                            false,
                        );
                    }
                    KeyCode::Char(' ') if is_multi => {
                        selected[cursor_idx] = !selected[cursor_idx];
                        draw_menu(
                            &mut stdout,
                            title,
                            items,
                            cursor_idx,
                            &selected,
                            is_multi,
                            false,
                        );
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
    first_draw: bool,
) {
    // Get terminal width so we can truncate lines to prevent wrapping.
    // Wrapping would break the MoveUp line count on redraw.
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);

    if !first_draw {
        // After the previous draw the cursor sits at the end of the hint
        // line (no trailing newline). Move up over: hint + items + title.
        let lines_up = (items.len() + 1) as u16;
        let _ = execute!(stdout, cursor::MoveUp(lines_up), Print("\r"));
    }
    let _ = execute!(stdout, terminal::Clear(ClearType::FromCursorDown));

    // Title — truncate to terminal width
    let title_trunc = truncate_str(title, term_width);
    let _ = execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(&title_trunc),
        ResetColor,
        Print("\r\n"),
    );

    // Items
    for (i, item) in items.iter().enumerate() {
        let is_focused = i == cursor_idx;
        let is_selected = selected[i];

        // Build the plain-text content of this line to measure/truncate.
        let prefix = if is_multi {
            let check = if is_selected { "[x]" } else { "[ ]" };
            if is_focused {
                format!("  {} ", check)
            } else {
                format!("  {} ", check)
            }
        } else if is_focused {
            "  > ".to_string()
        } else {
            "    ".to_string()
        };

        // Max chars available for label + description after the prefix
        let max_body = term_width.saturating_sub(prefix.len());

        // Print prefix with colors
        if is_multi {
            let check = if is_selected { "[x]" } else { "[ ]" };
            if is_focused {
                let _ = execute!(
                    stdout,
                    Print("  "),
                    SetForegroundColor(Color::Yellow),
                    Print(check),
                    Print(" "),
                    ResetColor,
                );
            } else {
                let _ = execute!(stdout, Print("  "), Print(check), Print(" "));
            }
        } else if is_focused {
            let _ = execute!(
                stdout,
                Print("  "),
                SetForegroundColor(Color::Yellow),
                Print("> "),
                ResetColor,
            );
        } else {
            let _ = execute!(stdout, Print("    "));
        }

        // Print body with colors
        let label_trunc = truncate_str(&item.label, max_body);
        if is_focused {
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::White),
                Print(&label_trunc),
                ResetColor,
            );
        } else {
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print(&label_trunc),
                ResetColor,
            );
        }

        if !item.description.is_empty() {
            let remaining = max_body.saturating_sub(item.label.len() + 2);
            if remaining > 0 {
                let desc_trunc = truncate_str(&item.description, remaining);
                let _ = execute!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print("  "),
                    Print(&desc_trunc),
                    ResetColor,
                );
            }
        }

        let _ = execute!(stdout, Print("\r\n"));
    }

    // Hint line
    let hint = if is_multi {
        "  ↑↓ navigate  ␣ toggle  ⏎ confirm  q cancel"
    } else {
        "  ↑↓ navigate  ⏎ select  q cancel"
    };
    let hint_trunc = truncate_str(hint, term_width);
    let _ = execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(&hint_trunc),
        ResetColor,
    );

    let _ = stdout.flush();
}

/// Truncate a string to at most `max_width` characters (by char count).
fn truncate_str(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        s.to_string()
    } else if max_width > 1 {
        let truncated: String = s.chars().take(max_width - 1).collect();
        format!("{}…", truncated)
    } else {
        s.chars().take(max_width).collect()
    }
}
