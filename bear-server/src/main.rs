mod llm;
mod lsp;
mod process;
mod relay;
mod rtc;
mod state;
mod tls_pin;
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
    CreateSessionRequest, CreateSessionResponse, RelayConfig, SessionListResponse, SessionStatus,
    DEFAULT_SERVER_URL,
};
use chrono::Utc;
use fs2::FileExt;
use std::{collections::HashMap, env, fs::OpenOptions, net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use base64::Engine as _;
use pkcs8::{EncodePrivateKey, EncodePublicKey};
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use sha2::{Digest, Sha256};

use llm::OllamaMessage;
use state::{AppConfig, LlmProvider, ServerState, Session, DEFAULT_BIND};

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install rustls CryptoProvider before any TLS/DTLS usage.
    // The webrtc crate needs this for the DTLS handshake that follows ICE.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _lock = acquire_server_lock()?;
    write_pid_file();

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
        relay_controller: Arc::new(relay::RelayController::new()),
    };

    // Auto-start relay polling if relay.json exists and relay is not disabled
    if bear_core::RelayConfig::exists() {
        let cfg = bear_core::ConfigFile::load();
        if cfg.relay_disabled != Some(true) {
            tracing::info!("relay.json found — starting relay polling");
            state.relay_controller.start(state.clone());
        } else {
            tracing::info!("relay.json found but relay is disabled in config");
        }
    }

    let app = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/ws/:session_id", get(ws_handler))
        .route("/rtc/:session_id/offer", post(rtc::rtc_offer))
        .route("/rtc/:session_id/ice/:conn_id", post(rtc::rtc_add_ice))
        .route(
            "/rtc/:session_id/candidates/:conn_id",
            post(rtc::rtc_get_candidates),
        )
        .route("/relay/pair", post(handle_relay_pair))
        .route("/relay/revoke", post(handle_relay_revoke))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods(Any),
        );

    let addr: SocketAddr = DEFAULT_BIND.parse()?;
    tracing::info!("bear-server running on http://{addr}");
    tracing::info!("default client url: {DEFAULT_SERVER_URL}");

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown on SIGTERM/SIGINT: delete PID file, then exit
    let server = axum::serve(listener, app);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let graceful = server.with_graceful_shutdown(async move {
            tokio::select! {
                _ = sigterm.recv() => {},
                _ = sigint.recv() => {},
            }
            tracing::info!("shutting down gracefully...");
            cleanup_pid_file();
        });
        graceful.await?;
    }
    #[cfg(not(unix))]
    {
        server.await?;
    }

    cleanup_pid_file();
    Ok(())
}

fn write_pid_file() {
    if let Some(path) = bear_core::server_pid_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, std::process::id().to_string());
    }
}

fn cleanup_pid_file() {
    if let Some(path) = bear_core::server_pid_path() {
        let _ = std::fs::remove_file(&path);
    }
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
// Relay pairing handlers
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RelayPairRequest {
    relay_url: String,
    invite_code: String,
}

async fn handle_relay_pair(
    State(state): State<ServerState>,
    Json(payload): Json<RelayPairRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match do_relay_pair(&state, payload).await {
        Ok(room_id) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "room_id": room_id })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn do_relay_pair(state: &ServerState, payload: RelayPairRequest) -> anyhow::Result<String> {
    // 1-3. Generate RSA-2048 keypair, export public key, hash invite code
    //      (done in spawn_blocking because RSA keygen is CPU-heavy and
    //       rsa types are not Send across await points)
    let invite_code = payload.invite_code.clone();
    let (pub_pem, priv_pem, room_id, hash_hex, jwt) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut rng = rand::thread_rng();
            let private_key = rsa::RsaPrivateKey::new(&mut rng, 2048)
                .map_err(|e| anyhow::anyhow!("RSA keygen failed: {e}"))?;
            let public_key = private_key.to_public_key();

            let pub_pem = public_key
                .to_public_key_pem(pkcs8::LineEnding::LF)
                .map_err(|e| anyhow::anyhow!("public key PEM export failed: {e}"))?;

            let hash_hex = hex_sha256(invite_code.as_bytes());
            let room_id = Uuid::new_v4().to_string();
            let jwt = mint_rs256_jwt(&private_key, &room_id, None)?;

            let priv_pem = private_key
                .to_pkcs8_pem(pkcs8::LineEnding::LF)
                .map_err(|e| anyhow::anyhow!("private key PEM export failed: {e}"))?;

            Ok((pub_pem, priv_pem.to_string(), room_id, hash_hex, jwt))
        })
        .await
        .map_err(|e| anyhow::anyhow!("keygen task panicked: {e}"))??;

    // 4. Capture relay TLS SPKI pin (only for HTTPS relays)
    let relay_tls_pin = if payload.relay_url.starts_with("https://") {
        match tls_pin::capture_spki_pin(&payload.relay_url).await {
            Ok(pin) => {
                tracing::info!("relay: captured TLS SPKI pin: {pin}");
                Some(pin)
            }
            Err(e) => {
                tracing::warn!("relay: failed to capture TLS pin (proceeding without): {e}");
                None
            }
        }
    } else {
        None
    };

    // 5. Call relay POST /pair
    let pair_url = format!("{}/pair", payload.relay_url.trim_end_matches('/'));
    let pair_body = serde_json::json!({
        "room_id": room_id,
        "signing_key": pub_pem,
        "invite_code": hash_hex,
    });
    let resp = state
        .http_client
        .post(&pair_url)
        .json(&pair_body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("relay /pair request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("relay /pair returned {status}: {body}"));
    }

    // 6. Save relay.json
    let relay_cfg = RelayConfig {
        relay_url: payload.relay_url,
        room_id: room_id.clone(),
        private_key_pem: priv_pem,
        jwt,
        relay_tls_pin,
    };
    relay_cfg
        .save()
        .map_err(|e| anyhow::anyhow!("failed to save relay.json: {e}"))?;

    // 6. Start relay polling
    tracing::info!("relay: paired successfully, room_id={room_id}");
    state.relay_controller.start(state.clone());

    Ok(room_id)
}

fn hex_sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn mint_rs256_jwt(
    private_key: &rsa::RsaPrivateKey,
    room_id: &str,
    ttl_secs: Option<i64>,
) -> anyhow::Result<String> {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let header_b64 = b64.encode(serde_json::to_vec(&header)?);

    let now = chrono::Utc::now().timestamp();
    let mut payload = serde_json::json!({ "room_id": room_id, "iat": now });
    if let Some(ttl) = ttl_secs {
        payload["exp"] = serde_json::json!(now + ttl);
    }
    let payload_b64 = b64.encode(serde_json::to_vec(&payload)?);

    let signing_input = format!("{header_b64}.{payload_b64}");

    let signing_key = SigningKey::<Sha256>::new(private_key.clone());
    let signature = signing_key.sign(signing_input.as_bytes());
    let sig_b64 = b64.encode(signature.to_bytes());

    Ok(format!("{signing_input}.{sig_b64}"))
}

async fn handle_relay_revoke(
    State(state): State<ServerState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match do_relay_revoke(&state).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn do_relay_revoke(state: &ServerState) -> anyhow::Result<()> {
    let relay_cfg = RelayConfig::load()
        .ok_or_else(|| anyhow::anyhow!("no relay.json found — not currently paired"))?;

    // Call relay POST /room/:room_id/revoke with JWT auth
    let revoke_url = format!(
        "{}/room/{}/revoke",
        relay_cfg.relay_url.trim_end_matches('/'),
        relay_cfg.room_id
    );
    let resp = state
        .http_client
        .post(&revoke_url)
        .header("Authorization", format!("Bearer {}", relay_cfg.jwt))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("relay /revoke request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("relay: revoke returned {status}: {body} — deleting local config anyway");
    }

    // Delete relay.json and stop polling
    let _ = RelayConfig::delete();
    state.relay_controller.stop();
    tracing::info!("relay: pairing revoked");

    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn list_sessions(State(state): State<ServerState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await;
    let items = sessions
        .values()
        .map(|session| session.info.clone())
        .collect();
    Json(SessionListResponse { sessions: items })
}

async fn create_session(
    State(state): State<ServerState>,
    Json(payload): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let cwd = payload.cwd.unwrap_or_else(|| {
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
        system_content.push_str(
            "\n\n## Project README\n\nThe working directory contains the following README.md:\n\n",
        );
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

    (
        StatusCode::CREATED,
        Json(CreateSessionResponse { session: info }),
    )
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
