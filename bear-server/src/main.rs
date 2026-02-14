mod llm;
mod process;
mod state;
mod tools;
mod ws;

use axum::{
    extract::{ws::WebSocketUpgrade, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
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
use state::{AppConfig, Session, ServerState, DEFAULT_BIND, SYSTEM_PROMPT};

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _lock = acquire_server_lock()?;

    let config = AppConfig::load_from_env();
    tracing::info!(
        "ollama configured: url={} model={}",
        config.ollama_url,
        config.ollama_model
    );

    let state = ServerState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        processes: Arc::new(RwLock::new(HashMap::new())),
        config,
        http_client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client"),
    };

    let app = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/ws/:session_id", get(ws_handler))
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
            content: SYSTEM_PROMPT.to_string(),
        }],
        undo_stack: Vec::new(),
    };

    let info = session.info.clone();
    state.sessions.write().await.insert(info.id, session);

    (StatusCode::CREATED, Json(CreateSessionResponse { session: info }))
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
