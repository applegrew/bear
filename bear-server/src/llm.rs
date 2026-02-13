use serde::{Deserialize, Serialize};
use std::env;
use tokio::sync::mpsc;

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

/// A single chunk from the Ollama streaming NDJSON response.
#[derive(Debug, Deserialize)]
struct OllamaStreamChunk {
    message: OllamaStreamMessage,
    #[serde(default)]
    done: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaStreamMessage {
    #[serde(default)]
    content: String,
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
// Streaming Ollama API call
// ---------------------------------------------------------------------------

/// Call Ollama with streaming enabled. Sends content chunks through `chunk_tx`
/// as they arrive. Returns the fully assembled OllamaMessage when done.
pub async fn call_ollama_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[OllamaMessage],
    chunk_tx: &mpsc::Sender<String>,
) -> anyhow::Result<OllamaMessage> {
    let url = format!("{}/api/chat", config.ollama_url.trim_end_matches('/'));
    let payload = OllamaChatRequest {
        model: config.ollama_model.clone(),
        messages: messages.to_vec(),
        stream: true,
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

    let mut full_content = String::new();
    let mut bytes_stream = response.bytes_stream();

    use futures::StreamExt;
    let mut buffer = String::new();

    while let Some(chunk_result) = bytes_stream.next().await {
        let chunk_bytes = chunk_result?;
        buffer.push_str(&String::from_utf8_lossy(&chunk_bytes));

        // Process complete NDJSON lines from the buffer
        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim().to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                continue;
            }

            if let Ok(chunk) = serde_json::from_str::<OllamaStreamChunk>(&line) {
                if !chunk.message.content.is_empty() {
                    full_content.push_str(&chunk.message.content);
                    let _ = chunk_tx.send(chunk.message.content).await;
                }
                if chunk.done {
                    return Ok(OllamaMessage {
                        role: "assistant".to_string(),
                        content: full_content,
                    });
                }
            }
        }
    }

    // If we get here without a done=true, return what we have
    Ok(OllamaMessage {
        role: "assistant".to_string(),
        content: full_content,
    })
}
