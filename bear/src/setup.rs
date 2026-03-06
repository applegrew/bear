use anyhow::Result;
use bear_core::ConfigFile;
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use std::io::{self, BufRead, Write};

use crate::menu::{interactive_menu_with_default, MenuItem, MenuMode, MenuResult};

/// Check if `~/.bear/config.json` exists. If not, run the setup wizard.
pub fn ensure_config() -> Result<()> {
    if ConfigFile::exists() {
        return Ok(());
    }
    run_setup_wizard(None)
}

/// Run the setup wizard explicitly (e.g. `--setup`), pre-filling with existing config.
pub fn rerun_setup() -> Result<()> {
    let existing = if ConfigFile::exists() {
        Some(ConfigFile::load())
    } else {
        None
    };
    run_setup_wizard(existing)
}

/// Interactive Q&A to create `~/.bear/config.json`.
/// If `existing` is provided, its values are used as defaults.
fn run_setup_wizard(existing: Option<ConfigFile>) -> Result<()> {
    let mut stdout = io::stdout();

    let heading = if existing.is_some() {
        "\nBear LLM configuration (current values shown as defaults).\n\n"
    } else {
        "\nWelcome to Bear! Let's set up your LLM configuration.\n\n"
    };

    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(heading),
        ResetColor,
    )?;

    // Determine initial provider index from existing config
    let initial_provider = existing
        .as_ref()
        .and_then(|c| c.llm_provider.as_deref())
        .map(|p| match p {
            "openai" => 1,
            "gemini" => 2,
            _ => 0,
        })
        .unwrap_or(0);

    // 1. Provider selection
    let items = vec![
        MenuItem {
            label: "Ollama (local)".to_string(),
            description: "Run models locally via Ollama".to_string(),
        },
        MenuItem {
            label: "OpenAI (or compatible API)".to_string(),
            description: "Use OpenAI, Azure, or any compatible endpoint".to_string(),
        },
        MenuItem {
            label: "Google Gemini".to_string(),
            description: "Use Google's Gemini API".to_string(),
        },
    ];

    let provider_idx = match interactive_menu_with_default(
        "Which LLM provider would you like to use?",
        &items,
        MenuMode::Single,
        initial_provider,
    ) {
        MenuResult::Single(idx) => idx,
        MenuResult::Cancelled => {
            anyhow::bail!("Setup cancelled.");
        }
        _ => 0,
    };

    // Start from existing config (preserves non-LLM fields like relay_disabled)
    // or default if no existing config.
    let mut config = existing.unwrap_or_default();

    match provider_idx {
        0 => {
            // Ollama
            config.llm_provider = Some("ollama".to_string());

            let def_url = config
                .ollama_url
                .as_deref()
                .unwrap_or("http://127.0.0.1:11434");
            let url = prompt_with_default("Ollama server URL", def_url)?;
            config.ollama_url = Some(url);

            let def_model = config.ollama_model.as_deref().unwrap_or("llama3.1");
            let model = prompt_with_default("Ollama model name", def_model)?;
            config.ollama_model = Some(model);
        }
        1 => {
            // OpenAI
            config.llm_provider = Some("openai".to_string());

            let api_key = prompt_with_existing("OpenAI API key", config.openai_api_key.as_deref())?;
            config.openai_api_key = Some(api_key);

            let def_model = config.openai_model.as_deref().unwrap_or("gpt-4");
            let model = prompt_with_default("OpenAI model", def_model)?;
            config.openai_model = Some(model);

            let def_url = config
                .openai_url
                .as_deref()
                .unwrap_or("https://api.openai.com");
            let url = prompt_with_default("OpenAI API URL", def_url)?;
            config.openai_url = Some(url);
        }
        _ => {
            // Gemini
            config.llm_provider = Some("gemini".to_string());

            let api_key = prompt_with_existing("Gemini API key", config.gemini_api_key.as_deref())?;
            config.gemini_api_key = Some(api_key);

            let def_model = config.gemini_model.as_deref().unwrap_or("gemini-2.0-flash");
            let model = prompt_with_default("Gemini model", def_model)?;
            config.gemini_model = Some(model);
        }
    }

    // -----------------------------------------------------------------------
    // Web search configuration
    // -----------------------------------------------------------------------

    execute!(
        stdout,
        Print("\n"),
        SetForegroundColor(Color::Cyan),
        Print("Web search (optional — press Enter to skip)\n\n"),
        ResetColor,
    )?;

    // Google Custom Search
    let google_key = prompt_optional(
        "Google API key",
        config.google_api_key.as_deref(),
    )?;
    config.google_api_key = google_key;

    if config.google_api_key.is_some() {
        let cx = prompt_optional(
            "Google Custom Search CX",
            config.google_cx.as_deref(),
        )?;
        config.google_cx = cx;
    }

    // Brave Search
    let brave_key = prompt_optional(
        "Brave Search API key",
        config.brave_api_key.as_deref(),
    )?;
    config.brave_api_key = brave_key;

    // -----------------------------------------------------------------------
    // Advanced settings
    // -----------------------------------------------------------------------

    execute!(
        stdout,
        Print("\n"),
        SetForegroundColor(Color::Cyan),
        Print("Advanced settings (press Enter to keep defaults)\n\n"),
        ResetColor,
    )?;

    let def_depth = config.max_tool_depth.unwrap_or(100).to_string();
    let depth_str = prompt_with_default("Max tool depth", &def_depth)?;
    config.max_tool_depth = depth_str.parse().ok();

    let def_output = config.max_tool_output_chars.unwrap_or(32_000).to_string();
    let output_str = prompt_with_default("Max tool output chars", &def_output)?;
    config.max_tool_output_chars = output_str.parse().ok();

    let def_budget = config.context_budget.unwrap_or(16_000).to_string();
    let budget_str = prompt_with_default("Context budget (tokens)", &def_budget)?;
    config.context_budget = budget_str.parse().ok();

    let def_recent = config.keep_recent.unwrap_or(20).to_string();
    let recent_str = prompt_with_default("Keep recent messages", &def_recent)?;
    config.keep_recent = recent_str.parse().ok();

    config.save()?;

    let path_display = bear_core::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.bear/config.json".to_string());

    execute!(
        stdout,
        Print("\n"),
        SetForegroundColor(Color::Green),
        Print(format!("Configuration saved to {path_display}\n")),
        ResetColor,
        Print("\n"),
    )?;

    Ok(())
}

/// Prompt the user for a value with a default. Empty input accepts the default.
fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let mut stdout = io::stdout();
    execute!(
        stdout,
        SetForegroundColor(Color::White),
        Print(format!("{label} [{default}]: ")),
        ResetColor,
    )?;
    stdout.flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Prompt for a sensitive value (e.g. API key) with an optional existing value.
/// If an existing value is present, shows a masked version and accepts Enter to keep it.
/// If no existing value, behaves like `prompt_required`.
fn prompt_with_existing(label: &str, existing: Option<&str>) -> Result<String> {
    let mut stdout = io::stdout();
    match existing {
        Some(val) if !val.is_empty() => {
            // Show masked value: first 4 chars + ****
            let masked = if val.len() > 4 {
                format!("{}****", &val[..4])
            } else {
                "****".to_string()
            };
            execute!(
                stdout,
                SetForegroundColor(Color::White),
                Print(format!("{label} [{masked}]: ")),
                ResetColor,
            )?;
            stdout.flush()?;

            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                Ok(val.to_string())
            } else {
                Ok(trimmed.to_string())
            }
        }
        _ => prompt_required(label),
    }
}

/// Prompt for an optional sensitive value (e.g. API key).
/// Shows masked existing value if present; Enter keeps it, typing "-" clears it,
/// empty input with no existing value skips.
fn prompt_optional(label: &str, existing: Option<&str>) -> Result<Option<String>> {
    let mut stdout = io::stdout();
    match existing {
        Some(val) if !val.is_empty() => {
            let masked = if val.len() > 4 {
                format!("{}****", &val[..4])
            } else {
                "****".to_string()
            };
            execute!(
                stdout,
                SetForegroundColor(Color::White),
                Print(format!("{label} [{masked}] (Enter=keep, -=clear): ")),
                ResetColor,
            )?;
            stdout.flush()?;
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                Ok(Some(val.to_string()))
            } else if trimmed == "-" {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        _ => {
            execute!(
                stdout,
                SetForegroundColor(Color::White),
                Print(format!("{label}: ")),
                ResetColor,
            )?;
            stdout.flush()?;
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
    }
}

/// Prompt the user for a required value. Repeats until non-empty input is given.
fn prompt_required(label: &str) -> Result<String> {
    let mut stdout = io::stdout();
    loop {
        execute!(
            stdout,
            SetForegroundColor(Color::White),
            Print(format!("{label} (required): ")),
            ResetColor,
        )?;
        stdout.flush()?;

        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        execute!(
            stdout,
            SetForegroundColor(Color::Red),
            Print("  This field is required. Please enter a value.\n"),
            ResetColor,
        )?;
    }
}
