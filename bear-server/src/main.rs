mod llm;
mod lsp;
mod process;
mod rtc;
mod state;
mod tool_bridge;
mod tools;
mod ws;

use axum::{
    extract::{ws::WebSocketUpgrade, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bear_core::{
    CreateSessionRequest, CreateSessionResponse, SessionListResponse, SessionStatus,
    DEFAULT_SERVER_URL,
};
use chrono::Utc;
use fs2::FileExt;
use std::{collections::HashMap, env, fs::OpenOptions, net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use llm::OllamaMessage;
use state::{AppConfig, Session, ServerState, DEFAULT_BIND, LlmProvider};

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _lock = acquire_server_lock()?;

    let config = AppConfig::load();
    let (provider_url, provider_model) = match config.llm_provider {
        LlmProvider::Ollama => (&config.ollama_url, &config.ollama_model),
        LlmProvider::OpenAI => (&config.openai_url, &config.openai_model),
    };
    tracing::info!(
        "{:?} configured: url={} model={}",
        config.llm_provider,
        provider_url,
        provider_model
    );

    let state = ServerState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        buses: Arc::new(RwLock::new(HashMap::new())),
        processes: Arc::new(RwLock::new(HashMap::new())),
        config,
        http_client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client"),
        rtc_peers: rtc::new_rtc_peers(),
        lsp_manager: Arc::new(lsp::LspManager::new()),
        workspace_store: Arc::new(bear_core::workspace::WorkspaceStore::new()),
    };

    let app = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/ws/:session_id", get(ws_handler))
        .route("/rtc/:session_id/offer", post(rtc::rtc_offer))
        .route("/rtc/:session_id/ice/:conn_id", post(rtc::rtc_add_ice))
        .route("/rtc/:session_id/candidates/:conn_id", post(rtc::rtc_get_candidates))
        .with_state(state)
        .layer(CorsLayer::new().allow_origin(Any).allow_headers(Any).allow_methods(Any));

    let addr: SocketAddr = DEFAULT_BIND.parse()?;
    tracing::info!("bear-server running on http://{addr}");
    tracing::info!("default client url: {DEFAULT_SERVER_URL}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn acquire_server_lock() -> anyhow::Result<std::fs::File> {
    let lock_path = dirs::home_dir()
        .unwrap_or_else(|| ".".into())
        .join(".bear")
        .join("server.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)?;

    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("bear-server already running (lock held)"))?;

    Ok(file)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn list_sessions(State(state): State<ServerState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    let items = sessions.values().map(|session| session.info.clone()).collect();
    Json(SessionListResponse { sessions: items })
}

async fn create_session(
    State(state): State<ServerState>,
    Json(payload): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let cwd = payload
        .cwd
        .unwrap_or_else(|| {
            env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| ".".to_string())
        });

    // Check if any LSP server is available on $PATH
    let lsp_available = lsp::any_lsp_server_available();
    if lsp_available {
        tracing::info!("LSP servers detected on $PATH — LSP tools enabled in system prompt");
    } else {
        tracing::info!("No LSP servers found on $PATH — LSP tools excluded from system prompt");
    }

    // Build system prompt, optionally enriched with README.md from the working directory
    let mut system_content = state::system_prompt(lsp_available);
    if let Some(readme) = read_readme_from_dir(&cwd) {
        system_content.push_str("\n\n## Project README\n\nThe working directory contains the following README.md:\n\n");
        system_content.push_str(&readme);
    }

    // Load persisted auto-approved set from .bear/ before moving cwd
    let auto_approved = state.workspace_store.load_auto_approved(&cwd).await;

    let session = Session {
        info: bear_core::SessionInfo {
            id: Uuid::new_v4(),
            name: None,
            cwd,
            created_at: Utc::now(),
            last_activity: Utc::now(),
            status: SessionStatus::Running,
        },
        history: vec![OllamaMessage {
            role: "system".to_string(),
            content: system_content,
        }],
        undo_stack: Vec::new(),
        todo_list: Vec::new(),
        input_history: Vec::new(),
        auto_approved,
        max_subagents: 3,
    };

    if !session.auto_approved.is_empty() {
        tracing::info!(
            "loaded {} auto-approved entries from .bear/",
            session.auto_approved.len()
        );
    }

    let info = session.info.clone();
    state.sessions.write().await.insert(info.id, session);

    (StatusCode::CREATED, Json(CreateSessionResponse { session: info }))
}

// ---------------------------------------------------------------------------
// README.md injection
// ---------------------------------------------------------------------------

const README_MAX_CHARS: usize = 4000;

/// Try to read a README.md (case-insensitive) from the given directory.
/// If the content exceeds `README_MAX_CHARS`, it is truncated with a note.
fn read_readme_from_dir(dir: &str) -> Option<String> {
    let dir_path = std::path::Path::new(dir);
    // Try common casing variants
    let candidates = ["README.md", "readme.md", "Readme.md", "README.MD"];
    let mut content = None;
    for name in &candidates {
        let path = dir_path.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            content = Some(text);
            break;
        }
    }
    let text = content?;
    if text.is_empty() {
        return None;
    }
    if text.len() <= README_MAX_CHARS {
        Some(text)
    } else {
        // Truncate at a line boundary near the limit
        let truncated = match text[..README_MAX_CHARS].rfind('\n') {
            Some(pos) => &text[..pos],
            None => &text[..README_MAX_CHARS],
        };
        Some(format!(
            "{}\n\n[… README truncated at {} chars out of {} total …]",
            truncated,
            truncated.len(),
            text.len(),
        ))
    }
}

// ---------------------------------------------------------------------------
// WebSocket upgrade handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    State(state): State<ServerState>,
    Path(session_id): Path<Uuid>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    let exists = {
        let sessions = state.sessions.read().await;
        sessions.contains_key(&session_id)
    };
    if !exists {
        return StatusCode::NOT_FOUND.into_response();
    }

    upgrade.on_upgrade(move |socket| ws::handle_socket(state, session_id, socket))
}
