use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{AppConfig, LlmProvider};

// ---------------------------------------------------------------------------
// Unified message format for both Ollama and OpenAI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Ollama message types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
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
    messages: &[ChatMessage],
) -> anyhow::Result<ChatMessage> {
    let url = format!("{}/api/chat", config.ollama_url.trim_end_matches('/'));
    let payload = OllamaChatRequest {
        model: config.ollama_model.clone(),
        messages: messages.to_vec(),
        stream: false,
    };

    #[derive(Debug, serde::Deserialize)]
    struct OllamaResp { message: OllamaRespMsg }
    #[derive(Debug, serde::Deserialize)]
    struct OllamaRespMsg { role: String, content: String }

    tracing::info!("ollama non-streaming request to {url} model={}", config.ollama_model);
    let response = http_client.post(&url).json(&payload).send().await?;
    tracing::info!("ollama non-streaming response status={}", response.status());
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("ollama returned {status}: {body}");
    }
    let body: OllamaResp = response.json().await?;
    Ok(ChatMessage {
        role: body.message.role,
        content: body.message.content,
    })
}

// ---------------------------------------------------------------------------
// Non-streaming OpenAI call
// ---------------------------------------------------------------------------

async fn call_openai(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
) -> anyhow::Result<ChatMessage> {
    let url = format!(
        "{}/v1/chat/completions",
        config.openai_url.trim_end_matches('/')
    );
    let openai_messages: Vec<OpenAiMessage> = messages
        .iter()
        .map(|m| OpenAiMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();
    let payload = OpenAiChatRequest {
        model: config.openai_model.clone(),
        messages: openai_messages,
        stream: false,
    };

    let mut req = http_client.post(&url).json(&payload);
    if let Some(ref key) = config.openai_api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    tracing::info!("openai non-streaming request to {url} model={}", config.openai_model);
    let response = req.send().await?;
    tracing::info!("openai non-streaming response status={}", response.status());
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("openai returned {status}: {body}");
    }
    let body: OpenAiChatResponse = response.json().await?;
    let msg = body
        .choices
        .into_iter()
        .next()
        .map(|c| ChatMessage {
            role: c.message.role,
            content: c.message.content,
        })
        .ok_or_else(|| anyhow::anyhow!("openai returned no choices"))?;
    Ok(msg)
}

// ---------------------------------------------------------------------------
// OpenAI streaming call
// ---------------------------------------------------------------------------

/// Call OpenAI with streaming enabled. Sends content chunks through `chunk_tx`
/// as they arrive. Returns the fully assembled ChatMessage when done.
pub async fn call_openai_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
    chunk_tx: &mpsc::Sender<String>,
) -> anyhow::Result<ChatMessage> {
    let url = format!(
        "{}/v1/chat/completions",
        config.openai_url.trim_end_matches('/')
    );
    let openai_messages: Vec<OpenAiMessage> = messages
        .iter()
        .map(|m| OpenAiMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();
    let payload = OpenAiChatRequest {
        model: config.openai_model.clone(),
        messages: openai_messages,
        stream: true,
    };

    let mut req = http_client.post(&url).json(&payload);
    if let Some(ref key) = config.openai_api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    tracing::info!("openai streaming request to {url} model={}", config.openai_model);
    let response = req.send().await?;
    tracing::info!("openai streaming response status={}", response.status());

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("openai returned {status}: {body}");
        anyhow::bail!("openai returned {status}: {body}");
    }

    let mut full_content = String::new();
    let mut bytes_stream = response.bytes_stream();

    use futures::StreamExt;
    let mut buffer = String::new();

    while let Some(chunk_result) = bytes_stream.next().await {
        let chunk_bytes = chunk_result?;
        let chunk_str = String::from_utf8_lossy(&chunk_bytes);
        buffer.push_str(&chunk_str);

        // Process complete SSE lines from the buffer
        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim().to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() || line == "data: [DONE]" {
                continue;
            }

            let json_str = line.strip_prefix("data: ").unwrap_or(&line);

            match serde_json::from_str::<OpenAiStreamChunk>(json_str) {
                Ok(chunk) => {
                    for choice in &chunk.choices {
                        if !choice.delta.content.is_empty() {
                            full_content.push_str(&choice.delta.content);
                            let _ = chunk_tx.send(choice.delta.content.clone()).await;
                        }
                        if choice.finish_reason.is_some() {
                            return Ok(ChatMessage {
                                role: "assistant".to_string(),
                                content: full_content,
                            });
                        }
                    }
                }
                Err(_err) => {
                    // Skip unparseable lines (e.g. comments, keep-alive)
                }
            }
        }
    }

    Ok(ChatMessage {
        role: "assistant".to_string(),
        content: full_content,
    })
}

// ---------------------------------------------------------------------------
// Unified LLM calls
// ---------------------------------------------------------------------------

/// Unified non-streaming LLM call that dispatches to the configured provider.
pub async fn call_llm_non_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
) -> anyhow::Result<ChatMessage> {
    match config.llm_provider {
        LlmProvider::Ollama => call_ollama(http_client, config, messages).await,
        LlmProvider::OpenAI => call_openai(http_client, config, messages).await,
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
        LlmProvider::Ollama => call_ollama_streaming(http_client, config, messages, chunk_tx).await,
        LlmProvider::OpenAI => call_openai_streaming(http_client, config, messages, chunk_tx).await,
    }
}

// ---------------------------------------------------------------------------
// Context compaction
// ---------------------------------------------------------------------------

const COMPACTION_PROMPT: &str = r#"You are a conversation summarizer. Given a conversation between a user and an AI coding assistant, produce a concise summary that captures:
1. What the user asked for
2. Key decisions made
3. Important code changes or file paths mentioned
4. Current state of the task (what's done, what's pending)

Be factual and specific. Include file paths, function names, and technical details. Keep it under 500 words."#;

/// If the history exceeds the token budget, compact older messages into a summary.
/// Layout after compaction: [system_prompt, summary_msg, ...recent_messages]
pub async fn compact_history_if_needed(
    http_client: &reqwest::Client,
    config: &AppConfig,
    history: &mut Vec<ChatMessage>,
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
    let rebuild = |history: &mut Vec<ChatMessage>, replacement: ChatMessage| {
        let system = history[0].clone();
        let recent: Vec<ChatMessage> = history[split_point..].to_vec();
        history.clear();
        history.push(system);
        history.push(replacement);
        history.extend(recent);
    };

    match call_llm_non_streaming(http_client, config, &summary_request).await {
        Ok(summary_reply) => {
            let summary_msg = ChatMessage {
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
            let fallback = ChatMessage {
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
    messages: &[ChatMessage],
    session_context: &str,
) -> anyhow::Result<ChatMessage> {
    let mut reflective_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: format!("{REFLECTION_PROMPT}\n\n{session_context}"),
    }];
    // Skip the original system prompt (messages[0]) to avoid two system messages
    if messages.len() > 1 {
        reflective_messages.extend_from_slice(&messages[1..]);
    }

    call_llm_non_streaming(http_client, config, &reflective_messages).await
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
    messages: &[ChatMessage],
    session_context: &str,
) -> anyhow::Result<String> {
    let mut planner_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: format!("{PLANNER_PROMPT}\n\n{session_context}"),
    }];
    // Include conversation history (skip original system prompt)
    if messages.len() > 1 {
        planner_messages.extend_from_slice(&messages[1..]);
    }
    let reply = call_llm_non_streaming(http_client, config, &planner_messages).await?;
    Ok(reply.content)
}

// ---------------------------------------------------------------------------
// Streaming Ollama API call
// ---------------------------------------------------------------------------

/// Call Ollama with streaming enabled. Sends content chunks through `chunk_tx`
/// as they arrive. Returns the fully assembled ChatMessage when done.
pub async fn call_ollama_streaming(
    http_client: &reqwest::Client,
    config: &AppConfig,
    messages: &[ChatMessage],
    chunk_tx: &mpsc::Sender<String>,
) -> anyhow::Result<ChatMessage> {
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
                        return Ok(ChatMessage {
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
    Ok(ChatMessage {
        role: "assistant".to_string(),
        content: full_content,
    })
}
