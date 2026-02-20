use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::state::{AppConfig, LlmProvider};

// ---------------------------------------------------------------------------
// Unified message format for both Ollama and OpenAI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// Conversion functions
impl From<ChatMessage> for OllamaMessage {
    fn from(msg: ChatMessage) -> Self {
        OllamaMessage {
            role: msg.role,
            content: msg.content,
        }
    }
}

impl From<OllamaMessage> for ChatMessage {
    fn from(msg: OllamaMessage) -> Self {
        ChatMessage {
            role: msg.role,
            content: msg.content,
        }
    }
}

// ---------------------------------------------------------------------------
// Ollama message types (for backward compatibility)
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
// OpenAI message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    stream: bool,
}

/// OpenAI API response for non-streaming calls
#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

/// A single chunk from the OpenAI streaming response.
#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamDelta {
    #[serde(default)]
    content: String,
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough token estimate: ~4 chars per token for English text.
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
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

    tracing::info!("ollama non-streaming request to {url} model={}", config.ollama_model);
    let response = http_client.post(&url).json(&payload).send().await?;
    tracing::info!("ollama non-streaming response status={}", response.status());
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("ollama returned {status}: {body}");
    }
    let body: Resp = response.json().await?;
    Ok(body.message)
}

// ---------------------------------------------------------------------------
// Non-streaming OpenAI call
// ---------------------------------------------------------------------------

async fn call_openai(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
) -> anyhow::Result<ChatMessage> {
    let api_key = config.openai_api_key.as_ref()
        .ok_or_else(|| anyhow::anyhow!("OpenAI API key not configured"))?;

    let openai_messages: Vec<OpenAiMessage> = messages.iter()
        .map(|msg| OpenAiMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
        })
        .collect();

    let payload = OpenAiChatRequest {
        model: config.openai_model.clone(),
        messages: openai_messages,
        stream: false,
    };

    tracing::info!("openai non-streaming request to {}/v1/chat/completions model={}", config.openai_url, config.openai_model);
    let response = http_client
        .post(&format!("{}/v1/chat/completions", config.openai_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    tracing::info!("openai non-streaming response status={}", response.status());
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI returned {status}: {body}");
    }

    let body: OpenAiChatResponse = response.json().await?;
    let choice = body.choices.first()
        .ok_or_else(|| anyhow::anyhow!("OpenAI returned no choices"))?;

    Ok(ChatMessage {
        role: choice.message.role.clone(),
        content: choice.message.content.clone(),
    })
}

// ---------------------------------------------------------------------------
// Streaming OpenAI API call
// ---------------------------------------------------------------------------

/// Call OpenAI with streaming enabled. Sends content chunks through `chunk_tx`
/// as they arrive. Returns the fully assembled ChatMessage when done.
pub async fn call_openai_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
    chunk_tx: &mpsc::Sender<String>,
) -> anyhow::Result<ChatMessage> {
    let api_key = config.openai_api_key.as_ref()
        .ok_or_else(|| anyhow::anyhow!("OpenAI API key not configured"))?;

    let openai_messages: Vec<OpenAiMessage> = messages.iter()
        .map(|msg| OpenAiMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
        })
        .collect();

    let payload = OpenAiChatRequest {
        model: config.openai_model.clone(),
        messages: openai_messages,
        stream: true,
    };

    tracing::info!("openai streaming request to {}/v1/chat/completions model={}", config.openai_url, config.openai_model);
    let response = http_client
        .post(&format!("{}/v1/chat/completions", config.openai_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    tracing::info!("openai streaming response status={}", response.status());

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("openai returned {status}: {body}");
        anyhow::bail!("openai returned {status}: {body}");
    }

    let mut full_content = String::new();

    use futures::StreamExt;
    let mut bytes_stream = response.bytes_stream();

    let mut raw_byte_count = 0usize;
    while let Some(chunk_result) = bytes_stream.next().await {
        let chunk_bytes = chunk_result?;
        raw_byte_count += chunk_bytes.len();
        let chunk_str = String::from_utf8_lossy(&chunk_bytes);
        if raw_byte_count <= 2000 {
            tracing::info!("openai stream raw ({} bytes): {:?}", chunk_bytes.len(), &chunk_str[..chunk_str.len().min(200)]);
        }

        // OpenAI streams as SSE (Server-Sent Events) format: "data: {...}\n\n"
        for line in chunk_str.lines() {
            let line = line.trim();
            if line.is_empty() || !line.starts_with("data: ") {
                continue;
            }

            let json_str = &line[6..]; // Remove "data: " prefix
            if json_str == "[DONE]" {
                tracing::info!("openai stream done: {raw_byte_count} bytes, content len={}", full_content.len());
                return Ok(ChatMessage {
                    role: "assistant".to_string(),
                    content: full_content,
                });
            }

            match serde_json::from_str::<OpenAiStreamChunk>(json_str) {
                Ok(chunk) => {
                    for choice in chunk.choices {
                        if !choice.delta.content.is_empty() {
                            full_content.push_str(&choice.delta.content);
                            let _ = chunk_tx.send(choice.delta.content).await;
                        }
                        if choice.finish_reason.is_some() {
                            tracing::info!("openai stream done: {raw_byte_count} bytes, content len={}", full_content.len());
                            return Ok(ChatMessage {
                                role: "assistant".to_string(),
                                content: full_content,
                            });
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!("openai stream JSON parse error: {err}, line: {:?}", &json_str[..json_str.len().min(300)]);
                }
            }
        }
    }

    tracing::info!("openai stream ended without finish_reason: {raw_byte_count} bytes, content len={}, remaining buffer", full_content.len());
    Ok(ChatMessage {
        role: "assistant".to_string(),
        content: full_content,
    })
}

// ---------------------------------------------------------------------------
// Unified LLM interface
// ---------------------------------------------------------------------------

/// Unified non-streaming LLM call that dispatches to the configured provider.
pub async fn call_llm_non_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
) -> anyhow::Result<ChatMessage> {
    match config.llm_provider {
        LlmProvider::Ollama => {
            let ollama_messages: Vec<OllamaMessage> = messages.iter()
                .map(|msg| msg.clone().into())
                .collect();
            call_ollama(http_client, config, &ollama_messages).await
                .map(|msg| msg.into())
        }
        LlmProvider::OpenAI => {
            call_openai(http_client, config, messages).await
        }
    }
}

/// Unified streaming LLM call that dispatches to the configured provider.
pub async fn call_llm_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
    chunk_tx: &mpsc::Sender<String>,
) -> anyhow::Result<ChatMessage> {
    match config.llm_provider {
        LlmProvider::Ollama => {
            let ollama_messages: Vec<OllamaMessage> = messages.iter()
                .map(|msg| msg.clone().into())
                .collect();
            call_ollama_streaming(http_client, config, &ollama_messages, chunk_tx).await
                .map(|msg| msg.into())
        }
        LlmProvider::OpenAI => {
            call_openai_streaming(http_client, config, messages, chunk_tx).await
        }
    }
}

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
    let chat_messages: Vec<ChatMessage> = history.iter()
        .map(|msg| msg.clone().into())
        .collect();
    let budget = config.context_budget;
    let tokens = estimate_tokens(&chat_messages);
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
        ChatMessage {
            role: "system".to_string(),
            content: COMPACTION_PROMPT.to_string(),
        },
        ChatMessage {
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

    match call_llm_non_streaming(http_client, config, &summary_request).await {
        Ok(summary_reply) => {
            let summary_msg = OllamaMessage {
                role: "user".to_string(),
                content: format!(
                    "[Session Context Summary — compacted from {old_count} earlier messages]\n\n{}",
                    summary_reply.content,
                ),
            };
            rebuild(history, summary_msg);

            let new_tokens = estimate_tokens(&chat_messages);
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
// Reflection and Planning
// ---------------------------------------------------------------------------

const REFLECTION_PROMPT: &str = r#"You are an expert software engineer and problem solver. Before responding to the user's request, take time to think through the problem carefully.

Consider:
- What exactly is the user asking for?
- What are the key challenges or complexities?
- What approach would be most effective?
- What potential pitfalls should be avoided?
- How can the solution be implemented cleanly and efficiently?

Structure your thinking clearly. After your analysis, provide your response to the user's request."#;

/// Ask the LLM to reflect on a problem before responding.
/// Makes a non-streaming call with a reflection system prompt, then appends the
/// reflection as an assistant message so the main LLM call benefits from it.
pub async fn reflective_thinking(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[OllamaMessage],
    session_context: &str,
) -> anyhow::Result<OllamaMessage> {
    let chat_messages: Vec<ChatMessage> = messages.iter()
        .map(|msg| msg.clone().into())
        .collect();

    let mut reflective_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: format!("{REFLECTION_PROMPT}\n\n{session_context}"),
    }];
    // Skip the original system prompt (messages[0]) to avoid two system messages
    if chat_messages.len() > 1 {
        reflective_messages.extend_from_slice(&chat_messages[1..]);
    }

    let result = call_llm_non_streaming(http_client, config, &reflective_messages).await?;
    Ok(result.into())
}

// ---------------------------------------------------------------------------
// Task Planning
// ---------------------------------------------------------------------------

const PLANNER_PROMPT: &str = r#"You are a task planner for an AI coding assistant called Bear. Given the user's message and conversation history, classify the request and optionally break it into sub-tasks.

You MUST respond with ONLY a JSON object (no markdown, no explanation) in this exact format:

{
  "type": "question" | "simple_task" | "complex_task",
  "plan": [
    { "id": "1", "description": "...", "needs_write": true },
    { "id": "2", "description": "...", "needs_write": false }
  ]
}

Rules:
- "question": The user is asking a question, seeking information, or wants an explanation. No tools needed. Plan should be empty [].
- "simple_task": A straightforward task that can be done in a few tool calls (e.g. read a file, run a command, make a small edit). Plan should be empty [].
- "complex_task": A multi-step task requiring several different actions (e.g. refactor code across files, implement a feature, debug a complex issue). Break it into 2-8 sub-tasks.

For each sub-task:
- "needs_write": true if the sub-task modifies files, runs commands, or changes state. false if it only reads/searches/explores.
- Keep descriptions concise but specific.
- Order tasks logically (exploration first, then modifications).

Examples of "question": "what does this function do?", "explain the architecture", "how should I approach X?"
Examples of "simple_task": "read src/main.rs", "run cargo build", "fix the typo on line 5"
Examples of "complex_task": "implement user authentication", "refactor the database layer to use connection pooling", "add tests for all API endpoints""#;

/// Classify a user prompt and optionally produce a task plan.
/// Returns a JSON string with the classification and plan.
pub async fn plan_task(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[OllamaMessage],
    session_context: &str,
) -> anyhow::Result<String> {
    let chat_messages: Vec<ChatMessage> = messages.iter()
        .map(|msg| msg.clone().into())
        .collect();

    let mut planner_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: format!("{PLANNER_PROMPT}\n\n{session_context}"),
    }];
    // Include conversation history (skip original system prompt)
    if chat_messages.len() > 1 {
        planner_messages.extend_from_slice(&chat_messages[1..]);
    }
    let reply = call_llm_non_streaming(http_client, config, &planner_messages).await?;
    Ok(reply.content)
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

    tracing::info!("ollama streaming request to {url} model={}", config.ollama_model);
    let response = http_client
        .post(&url)
        .json(&payload)
        .send()
        .await?;
    tracing::info!("ollama streaming response status={}", response.status());

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

    let mut raw_byte_count = 0usize;
    let mut line_count = 0usize;
    while let Some(chunk_result) = bytes_stream.next().await {
        let chunk_bytes = chunk_result?;
        raw_byte_count += chunk_bytes.len();
        let chunk_str = String::from_utf8_lossy(&chunk_bytes);
        if raw_byte_count <= 2000 {
            tracing::info!("ollama stream raw ({} bytes): {:?}", chunk_bytes.len(), &chunk_str[..chunk_str.len().min(200)]);
        }
        buffer.push_str(&chunk_str);

        // Process complete NDJSON lines from the buffer
        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim().to_string();
            buffer = buffer[newline_pos + 1..].to_string();
            line_count += 1;

            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<OllamaStreamChunk>(&line) {
                Ok(chunk) => {
                    if !chunk.message.content.is_empty() {
                        full_content.push_str(&chunk.message.content);
                        let _ = chunk_tx.send(chunk.message.content).await;
                    }
                    if chunk.done {
                        tracing::info!("ollama stream done: {raw_byte_count} bytes, {line_count} lines, content len={}", full_content.len());
                        return Ok(OllamaMessage {
                            role: "assistant".to_string(),
                            content: full_content,
                        });
                    }
                }
                Err(err) => {
                    tracing::warn!("ollama stream JSON parse error: {err}, line: {:?}", &line[..line.len().min(300)]);
                }
            }
        }
    }

    tracing::info!("ollama stream ended without done=true: {raw_byte_count} bytes, {line_count} lines, content len={}, remaining buffer={:?}", full_content.len(), &buffer[..buffer.len().min(500)]);
    // If we get here without a done=true, return what we have
    Ok(OllamaMessage {
        role: "assistant".to_string(),
        content: full_content,
    })
}
