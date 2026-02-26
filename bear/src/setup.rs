use anyhow::Result;
use bear_core::ConfigFile;
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use std::io::{self, BufRead, Write};

use crate::menu::{interactive_menu, MenuItem, MenuMode, MenuResult};

/// Check if `~/.bear/config.json` exists. If not, run the setup wizard.
pub fn ensure_config() -> Result<()> {
    if ConfigFile::exists() {
        return Ok(());
    }
    run_setup_wizard()
}

/// Interactive Q&A to create `~/.bear/config.json`.
fn run_setup_wizard() -> Result<()> {
    let mut stdout = io::stdout();

    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print("\nWelcome to Bear! Let's set up your LLM configuration.\n\n"),
        ResetColor,
    )?;

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
    ];

    let provider_idx = match interactive_menu(
        "Which LLM provider would you like to use?",
        &items,
        MenuMode::Single,
    ) {
        MenuResult::Single(idx) => idx,
        MenuResult::Cancelled => {
            anyhow::bail!("Setup cancelled.");
        }
        _ => 0,
    };

    let mut config = ConfigFile::default();

    if provider_idx == 0 {
        // Ollama
        config.llm_provider = Some("ollama".to_string());

        let url = prompt_with_default("Ollama server URL", "http://127.0.0.1:11434")?;
        config.ollama_url = Some(url);

        let model = prompt_with_default("Ollama model name", "llama3.1")?;
        config.ollama_model = Some(model);
    } else {
        // OpenAI
        config.llm_provider = Some("openai".to_string());

        let api_key = prompt_required("OpenAI API key")?;
        config.openai_api_key = Some(api_key);

        let model = prompt_with_default("OpenAI model", "gpt-4")?;
        config.openai_model = Some(model);

        let url = prompt_with_default("OpenAI API URL", "https://api.openai.com")?;
        config.openai_url = Some(url);
    }

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
