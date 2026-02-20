// ---------------------------------------------------------------------------
// LLM provider & application configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LlmProvider {
    Ollama,
    OpenAI,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub llm_provider: LlmProvider,
    pub ollama_url: String,
    pub ollama_model: String,
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_url: String,
    pub max_tool_depth: usize,
    pub max_tool_output_chars: usize,
    pub context_budget: usize,
    pub keep_recent: usize,
}

impl AppConfig {
    pub fn load_from_env() -> Self {
        fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }

        let provider_str = std::env::var("BEAR_LLM_PROVIDER").unwrap_or_else(|_| "ollama".to_string());
        let llm_provider = match provider_str.to_lowercase().as_str() {
            "openai" => LlmProvider::OpenAI,
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
            max_tool_depth: env_or("BEAR_MAX_TOOL_DEPTH", 100),
            max_tool_output_chars: env_or("BEAR_MAX_TOOL_OUTPUT_CHARS", 32_000),
            context_budget: env_or("BEAR_CONTEXT_BUDGET", 16_000),
            keep_recent: env_or("BEAR_KEEP_RECENT", 20),
        }
    }
}
