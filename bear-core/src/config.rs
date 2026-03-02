// ---------------------------------------------------------------------------
// LLM provider & application configuration
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum LlmProvider {
    Ollama,
    OpenAI,
    Gemini,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub llm_provider: LlmProvider,
    pub ollama_url: String,
    pub ollama_model: String,
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_url: String,
    pub gemini_api_key: Option<String>,
    pub gemini_model: String,
    pub max_tool_depth: usize,
    pub max_tool_output_chars: usize,
    pub context_budget: usize,
    pub keep_recent: usize,
    // Web search fallback keys
    pub google_api_key: Option<String>,
    pub google_cx: Option<String>,
    pub brave_api_key: Option<String>,
}

/// On-disk representation of `~/.bear/config.json`. All fields are optional —
/// missing/null values fall through to env vars, then built-in defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    pub llm_provider: Option<String>,
    pub ollama_url: Option<String>,
    pub ollama_model: Option<String>,
    pub openai_api_key: Option<String>,
    pub openai_model: Option<String>,
    pub openai_url: Option<String>,
    pub gemini_api_key: Option<String>,
    pub gemini_model: Option<String>,
    pub max_tool_depth: Option<usize>,
    pub max_tool_output_chars: Option<usize>,
    pub context_budget: Option<usize>,
    pub keep_recent: Option<usize>,
    pub google_api_key: Option<String>,
    pub google_cx: Option<String>,
    pub brave_api_key: Option<String>,
    pub relay_disabled: Option<bool>,
}

/// Returns the path to `~/.bear/config.json`.
pub fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bear").join("config.json"))
}

/// Returns the path to `~/.bear/relay.json`.
pub fn relay_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bear").join("relay.json"))
}

/// Returns the path to `~/.bear/server.pid`.
pub fn server_pid_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bear").join("server.pid"))
}

/// On-disk representation of `~/.bear/relay.json` — relay pairing credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    pub relay_url: String,
    pub room_id: String,
    pub private_key_pem: String,
    pub jwt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_tls_pin: Option<String>,
}

impl RelayConfig {
    /// Read relay config from disk. Returns `None` if the file doesn't exist or is invalid.
    pub fn load() -> Option<Self> {
        let path = relay_path()?;
        let contents = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Write relay config to disk, creating `~/.bear/` if needed.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = relay_path() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine home directory",
            ));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, json)
    }

    /// Delete the relay config file from disk.
    pub fn delete() -> std::io::Result<()> {
        if let Some(path) = relay_path() {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    /// Returns true if the relay config file exists on disk.
    pub fn exists() -> bool {
        relay_path().map(|p| p.exists()).unwrap_or(false)
    }
}

impl ConfigFile {
    /// Read the config file from disk. Returns `Default` if the file doesn't exist.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write the config file to disk, creating `~/.bear/` if needed.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = config_path() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine home directory",
            ));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, json)
    }

    /// Returns true if the config file exists on disk.
    pub fn exists() -> bool {
        config_path().map(|p| p.exists()).unwrap_or(false)
    }
}

impl AppConfig {
    /// Load configuration with priority: env vars > config file > built-in defaults.
    pub fn load() -> Self {
        let file = ConfigFile::load();

        fn env_or<T: std::str::FromStr>(key: &str, file_val: Option<T>, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file_val)
                .unwrap_or(default)
        }

        fn env_or_string(key: &str, file_val: Option<String>, default: &str) -> String {
            std::env::var(key)
                .ok()
                .or(file_val)
                .unwrap_or_else(|| default.to_string())
        }

        fn env_or_opt(key: &str, file_val: Option<String>) -> Option<String> {
            std::env::var(key).ok().or(file_val)
        }

        let provider_str = env_or_string("BEAR_LLM_PROVIDER", file.llm_provider, "ollama");
        let llm_provider = match provider_str.to_lowercase().as_str() {
            "openai" => LlmProvider::OpenAI,
            "gemini" => LlmProvider::Gemini,
            _ => LlmProvider::Ollama,
        };

        Self {
            llm_provider,
            ollama_url: env_or_string("BEAR_OLLAMA_URL", file.ollama_url, "http://127.0.0.1:11434"),
            ollama_model: env_or_string("BEAR_OLLAMA_MODEL", file.ollama_model, "llama3.1"),
            openai_api_key: env_or_opt("BEAR_OPENAI_API_KEY", file.openai_api_key),
            openai_model: env_or_string("BEAR_OPENAI_MODEL", file.openai_model, "gpt-4"),
            openai_url: env_or_string("BEAR_OPENAI_URL", file.openai_url, "https://api.openai.com"),
            gemini_api_key: env_or_opt("BEAR_GEMINI_API_KEY", file.gemini_api_key),
            gemini_model: env_or_string("BEAR_GEMINI_MODEL", file.gemini_model, "gemini-2.0-flash"),
            max_tool_depth: env_or("BEAR_MAX_TOOL_DEPTH", file.max_tool_depth, 100),
            max_tool_output_chars: env_or(
                "BEAR_MAX_TOOL_OUTPUT_CHARS",
                file.max_tool_output_chars,
                32_000,
            ),
            context_budget: env_or("BEAR_CONTEXT_BUDGET", file.context_budget, 16_000),
            keep_recent: env_or("BEAR_KEEP_RECENT", file.keep_recent, 20),
            google_api_key: env_or_opt("BEAR_GOOGLE_API_KEY", file.google_api_key),
            google_cx: env_or_opt("BEAR_GOOGLE_CX", file.google_cx),
            brave_api_key: env_or_opt("BEAR_BRAVE_API_KEY", file.brave_api_key),
        }
    }

    /// Legacy loader — env vars + built-in defaults only (no config file).
    pub fn load_from_env() -> Self {
        fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }

        let provider_str =
            std::env::var("BEAR_LLM_PROVIDER").unwrap_or_else(|_| "ollama".to_string());
        let llm_provider = match provider_str.to_lowercase().as_str() {
            "openai" => LlmProvider::OpenAI,
            "gemini" => LlmProvider::Gemini,
            _ => LlmProvider::Ollama,
        };

        Self {
            llm_provider,
            ollama_url: std::env::var("BEAR_OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            ollama_model: std::env::var("BEAR_OLLAMA_MODEL")
                .unwrap_or_else(|_| "llama3.1".to_string()),
            openai_api_key: std::env::var("BEAR_OPENAI_API_KEY").ok(),
            openai_model: std::env::var("BEAR_OPENAI_MODEL")
                .unwrap_or_else(|_| "gpt-4".to_string()),
            openai_url: std::env::var("BEAR_OPENAI_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string()),
            gemini_api_key: std::env::var("BEAR_GEMINI_API_KEY").ok(),
            gemini_model: std::env::var("BEAR_GEMINI_MODEL")
                .unwrap_or_else(|_| "gemini-2.0-flash".to_string()),
            max_tool_depth: env_or("BEAR_MAX_TOOL_DEPTH", 100),
            max_tool_output_chars: env_or("BEAR_MAX_TOOL_OUTPUT_CHARS", 32_000),
            context_budget: env_or("BEAR_CONTEXT_BUDGET", 16_000),
            keep_recent: env_or("BEAR_KEEP_RECENT", 20),
            google_api_key: std::env::var("BEAR_GOOGLE_API_KEY").ok(),
            google_cx: std::env::var("BEAR_GOOGLE_CX").ok(),
            brave_api_key: std::env::var("BEAR_BRAVE_API_KEY").ok(),
        }
    }
}
