use serde::{Deserialize, Serialize};
use std::env;

use crate::state::AppConfig;

// ---------------------------------------------------------------------------
// Ollama message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub fn load_config() -> AppConfig {
    let ollama_url = env::var("BEAR_OLLAMA_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    let ollama_model = env::var("BEAR_OLLAMA_MODEL")
        .unwrap_or_else(|_| "llama3.1".to_string());
    AppConfig {
        ollama_url,
        ollama_model,
    }
}

// ---------------------------------------------------------------------------
// Ollama API call
// ---------------------------------------------------------------------------

pub async fn call_ollama(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[OllamaMessage],
) -> anyhow::Result<OllamaMessage> {
    let url = format!("{}/api/chat", config.ollama_url.trim_end_matches('/'));
    let payload = OllamaChatRequest {
        model: config.ollama_model.clone(),
        messages: messages.to_vec(),
        stream: false,
    };

    let response = http_client
        .post(&url)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("ollama returned {status}: {body}");
        anyhow::bail!("ollama returned {status}: {body}");
    }

    let body: OllamaChatResponse = response.json().await?;
    Ok(body.message)
}
