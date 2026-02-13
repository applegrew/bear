use serde::{Deserialize, Serialize};
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
// Token estimation
// ---------------------------------------------------------------------------

/// Rough token estimate: ~4 chars per token for English text.
pub fn estimate_tokens(messages: &[OllamaMessage]) -> usize {
    messages.iter().map(|m| m.content.len() / 4 + 1).sum()
}

// ---------------------------------------------------------------------------
// Non-streaming Ollama call (used internally for compaction)
// ---------------------------------------------------------------------------

async fn call_ollama(
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

    #[derive(Debug, serde::Deserialize)]
    struct Resp { message: OllamaMessage }

    let response = http_client.post(&url).json(&payload).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("ollama returned {status}: {body}");
    }
    let body: Resp = response.json().await?;
    Ok(body.message)
}

// ---------------------------------------------------------------------------
// Context compaction
// ---------------------------------------------------------------------------

const COMPACTION_PROMPT: &str = r#"You are a summarization assistant. Summarize the following conversation between a user and an AI coding assistant called Bear. Preserve:
- The user's original goal and intent
- Key technical decisions and architectural choices
- File paths, function names, and important code structures discussed
- What was accomplished and what remains to be done
- Any errors encountered and how they were resolved

Be concise but thorough. This summary will replace the original messages to save context space."#;

/// If the history exceeds the token budget, compact older messages into a summary.
/// Layout after compaction: [system_prompt, summary_msg, ...recent_messages]
pub async fn compact_history_if_needed(
    http_client: &reqwest::Client,
    config: &AppConfig,
    history: &mut Vec<OllamaMessage>,
) {
    let budget = config.context_budget;
    let tokens = estimate_tokens(history);
    if tokens <= budget {
        return;
    }

    let keep = config.keep_recent.min(history.len().saturating_sub(1));
    // We need at least the system prompt + 2 messages to compact anything
    if history.len() <= keep + 2 {
        return;
    }

    tracing::info!(
        "context compaction triggered: {} tokens > {} budget, {} messages",
        tokens, budget, history.len()
    );

    // Split: [system_prompt] [old_messages_to_summarize...] [recent_to_keep...]
    let split_point = history.len() - keep;
    let old_count = split_point - 1; // number of messages to summarize (skip system)

    if old_count == 0 {
        return;
    }

    // Build the conversation text to summarize (consumes the borrow before mutation)
    let mut conversation_text = String::new();
    let mut old_tokens = 0usize;
    for msg in &history[1..split_point] {
        let role = &msg.role;
        old_tokens += msg.content.len() / 4 + 1;
        let content = if msg.content.len() > 2000 {
            format!("{}\n[... truncated ...]", &msg.content[..2000])
        } else {
            msg.content.clone()
        };
        conversation_text.push_str(&format!("[{role}]: {content}\n\n"));
    }

    let summary_request = vec![
        OllamaMessage {
            role: "system".to_string(),
            content: COMPACTION_PROMPT.to_string(),
        },
        OllamaMessage {
            role: "user".to_string(),
            content: format!(
                "Summarize this conversation ({old_count} messages, ~{old_tokens} tokens):\n\n{conversation_text}",
            ),
        },
    ];

    // Helper to rebuild history from system + replacement + recent
    let rebuild = |history: &mut Vec<OllamaMessage>, replacement: OllamaMessage| {
        let system = history[0].clone();
        let recent: Vec<OllamaMessage> = history[split_point..].to_vec();
        history.clear();
        history.push(system);
        history.push(replacement);
        history.extend(recent);
    };

    match call_ollama(http_client, config, &summary_request).await {
        Ok(summary_reply) => {
            let summary_msg = OllamaMessage {
                role: "user".to_string(),
                content: format!(
                    "[Session Context Summary — compacted from {old_count} earlier messages]\n\n{}",
                    summary_reply.content,
                ),
            };
            rebuild(history, summary_msg);

            let new_tokens = estimate_tokens(history);
            tracing::info!(
                "compaction complete: {} -> {} messages, {} -> {} est. tokens",
                split_point, history.len(), tokens, new_tokens,
            );
        }
        Err(err) => {
            tracing::warn!("compaction summarization failed, skipping: {err}");
            let fallback = OllamaMessage {
                role: "user".to_string(),
                content: format!(
                    "[Session Context — {old_count} earlier messages were dropped due to context limits. \
                     Some context may be missing.]",
                ),
            };
            rebuild(history, fallback);
        }
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
