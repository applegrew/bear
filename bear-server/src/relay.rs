// ---------------------------------------------------------------------------
// Relay polling — polls a remote relay server for WebRTC signaling offers
// ---------------------------------------------------------------------------
//
// When `~/.bear/relay.json` exists and relay is not disabled, this module
// spawns a background task that:
//   1. Polls GET /room/:room_id/offer on the relay
//   2. When an offer arrives, creates an RTCPeerConnection + answer
//   3. POSTs the answer back, exchanges ICE candidates
//   4. Reports relay status to all connected clients via the session bus
//
// The polling task is controlled via RelayStart / RelayStop client messages.

use std::sync::Arc;
use std::time::Duration;

use bear_core::{ConfigFile, RelayConfig, ServerMessage};
use tokio::sync::{watch, Mutex};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::state::ServerState;

// ---------------------------------------------------------------------------
// Relay status — broadcast to all sessions
// ---------------------------------------------------------------------------

fn relay_status(status: &str, detail: Option<String>) -> ServerMessage {
    ServerMessage::RelayStatus {
        status: status.to_string(),
        detail,
    }
}

// ---------------------------------------------------------------------------
// Relay controller — manages the polling task lifecycle
// ---------------------------------------------------------------------------

/// Shared relay state, held in ServerState.
pub struct RelayController {
    /// Send `true` to start polling, `false` to stop.
    cmd_tx: watch::Sender<bool>,
    /// The relay polling task handle.
    handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl RelayController {
    pub fn new() -> Self {
        let (cmd_tx, _) = watch::channel(false);
        Self {
            cmd_tx,
            handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Start relay polling (if not already running).
    /// Uses a spawned task to break the async opaque type cycle between
    /// relay → rtc → ws → session_worker → relay_controller.
    pub fn start(&self, state: ServerState) {
        let cmd_tx = self.cmd_tx.clone();
        let handle_arc = self.handle.clone();
        tokio::spawn(async move {
            let mut handle = handle_arc.lock().await;
            if handle.as_ref().map_or(false, |h| !h.is_finished()) {
                let _ = cmd_tx.send(true);
                return;
            }
            let _ = cmd_tx.send(true);
            let cmd_rx = cmd_tx.subscribe();
            let task = tokio::spawn(relay_poll_loop(state, cmd_rx));
            *handle = Some(task);
        });
    }

    /// Stop relay polling.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(false);
        let handle_arc = self.handle.clone();
        tokio::spawn(async move {
            let mut handle = handle_arc.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Polling loop
// ---------------------------------------------------------------------------

async fn relay_poll_loop(state: ServerState, mut cmd_rx: watch::Receiver<bool>) {
    let default_interval = Duration::from_secs(
        std::env::var("BEAR_RELAY_POLL_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
    );

    let mut backoff_count: u32 = 0;
    let max_backoff = Duration::from_secs(300); // 5 minutes

    loop {
        // Check if we should be running
        if !*cmd_rx.borrow() {
            tracing::info!("relay: polling stopped by command");
            break;
        }

        // Load relay config
        let relay_cfg = match RelayConfig::load() {
            Some(cfg) => cfg,
            None => {
                tracing::info!("relay: no relay.json found, polling disabled");
                broadcast_all_sessions(&state, relay_status("disabled", None)).await;
                break;
            }
        };

        // Check if relay is disabled in config
        let config_file = ConfigFile::load();
        if config_file.relay_disabled == Some(true) {
            tracing::info!("relay: disabled in config");
            broadcast_all_sessions(&state, relay_status("disabled", Some("disabled in config".into()))).await;
            break;
        }

        // Poll for offers
        let poll_url = format!("{}/room/{}/offer", relay_cfg.relay_url, relay_cfg.room_id);
        let result = state
            .http_client
            .get(&poll_url)
            .header("Authorization", format!("Bearer {}", relay_cfg.jwt))
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status_code = resp.status().as_u16();
                match status_code {
                    200 => {
                        // Reset backoff on success
                        if backoff_count > 0 {
                            backoff_count = 0;
                            broadcast_all_sessions(&state, relay_status("connected", None)).await;
                        }

                        // Parse offer
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            let conn_id = body["conn_id"].as_str().unwrap_or_default().to_string();
                            let sdp = body["sdp"].as_str().unwrap_or_default().to_string();

                            if !conn_id.is_empty() && !sdp.is_empty() {
                                tracing::info!("relay: received offer conn_id={conn_id}");
                                tokio::spawn(handle_relay_offer(
                                    state.clone(),
                                    relay_cfg.clone(),
                                    conn_id,
                                    sdp,
                                ));
                            }
                        }
                    }
                    204 => {
                        // No pending offers — normal
                        if backoff_count > 0 {
                            backoff_count = 0;
                            broadcast_all_sessions(&state, relay_status("connected", None)).await;
                        }
                    }
                    401 | 404 => {
                        // Revoked or room not found
                        tracing::warn!("relay: poll returned {status_code} — pairing revoked");
                        broadcast_all_sessions(
                            &state,
                            relay_status("revoked", Some(format!("relay returned {status_code}"))),
                        )
                        .await;
                        // Delete relay.json
                        let _ = RelayConfig::delete();
                        break;
                    }
                    429 => {
                        // Rate limited — back off
                        tracing::warn!("relay: rate limited (429)");
                        backoff_count = backoff_count.saturating_add(1);
                    }
                    _ => {
                        tracing::warn!("relay: unexpected status {status_code}");
                        backoff_count = backoff_count.saturating_add(1);
                    }
                }
            }
            Err(e) => {
                backoff_count = backoff_count.saturating_add(1);
                let delay = backoff_delay(backoff_count, default_interval, max_backoff);
                tracing::warn!(
                    "relay: poll failed (attempt {backoff_count}): {e} — retrying in {}s",
                    delay.as_secs()
                );
                broadcast_all_sessions(
                    &state,
                    relay_status(
                        "backoff",
                        Some(format!(
                            "attempt {}, next retry in {}s: {}",
                            backoff_count,
                            delay.as_secs(),
                            e
                        )),
                    ),
                )
                .await;

                // Wait with cancellation check
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = cmd_rx.changed() => {
                        if !*cmd_rx.borrow() {
                            break;
                        }
                    }
                }
                continue;
            }
        }

        // Normal poll interval
        tokio::select! {
            _ = tokio::time::sleep(default_interval) => {},
            _ = cmd_rx.changed() => {
                if !*cmd_rx.borrow() {
                    break;
                }
            }
        }
    }
}

fn backoff_delay(count: u32, base: Duration, max: Duration) -> Duration {
    let secs = base.as_secs().saturating_mul(1u64.checked_shl(count.min(10)).unwrap_or(u64::MAX));
    Duration::from_secs(secs).min(max)
}

/// Broadcast a message to all active session buses.
async fn broadcast_all_sessions(state: &ServerState, msg: ServerMessage) {
    let buses = state.buses.read().await;
    for bus in buses.values() {
        bus.topic.push(msg.clone()).await;
    }
}

// ---------------------------------------------------------------------------
// Handle a single relay offer — create peer connection, exchange signaling
// ---------------------------------------------------------------------------

async fn handle_relay_offer(
    state: ServerState,
    relay_cfg: RelayConfig,
    conn_id: String,
    sdp_offer: String,
) {
    // Pick the first available session to bind this connection to.
    // In the one-server-per-account model there's typically one active session.
    let session_info = {
        let sessions = state.sessions.read().await;
        sessions.values().next().map(|s| s.info.clone())
    };
    let Some(info) = session_info else {
        tracing::warn!("relay: offer received but no active sessions");
        return;
    };
    let session_id = info.id;

    // Verify the session has an active bus (worker must already be running)
    {
        let buses = state.buses.read().await;
        if !buses.contains_key(&session_id) {
            tracing::warn!("relay: session {session_id} has no active bus, skipping offer");
            return;
        }
    }

    // Build WebRTC peer connection
    let mut media_engine = MediaEngine::default();
    if let Err(e) = media_engine.register_default_codecs() {
        tracing::error!("relay: failed to register codecs: {e}");
        return;
    }

    let mut registry = Registry::new();
    registry = match register_default_interceptors(registry, &mut media_engine) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("relay: interceptor init failed: {e}");
            return;
        }
    };

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec![
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun1.l.google.com:19302".to_string(),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    let pc = match api.new_peer_connection(config).await {
        Ok(pc) => Arc::new(pc),
        Err(e) => {
            tracing::error!("relay: failed to create peer connection: {e}");
            return;
        }
    };

    // Collect server ICE candidates to POST to relay
    let server_candidates: Arc<tokio::sync::Mutex<Vec<RTCIceCandidateInit>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let cands = server_candidates.clone();
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        let cands = cands.clone();
        Box::pin(async move {
            if let Some(c) = candidate {
                if let Ok(init) = c.to_json() {
                    cands.lock().await.push(init);
                }
            }
        })
    }));

    // When the remote peer creates a DataChannel, start relaying
    let relay_state = state.clone();
    let relay_info = info.clone();
    pc.on_data_channel(Box::new(move |dc| {
        let state = relay_state.clone();
        let sid = session_id;
        let info = relay_info.clone();
        Box::pin(async move {
            tracing::info!("relay: data channel '{}' opened for session {sid}", dc.label());
            tokio::spawn(async move {
                crate::rtc::handle_relay_data_channel(state, sid, info, dc).await;
            });
        })
    }));

    // Set remote description (browser's offer)
    let offer = RTCSessionDescription::offer(sdp_offer).unwrap();
    if let Err(e) = pc.set_remote_description(offer).await {
        tracing::error!("relay: set_remote_description failed: {e}");
        return;
    }

    // Create answer
    let answer = match pc.create_answer(None).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("relay: create_answer failed: {e}");
            return;
        }
    };

    let sdp_answer = answer.sdp.clone();
    if let Err(e) = pc.set_local_description(answer).await {
        tracing::error!("relay: set_local_description failed: {e}");
        return;
    }

    // POST answer to relay
    let answer_url = format!(
        "{}/room/{}/answer/{}",
        relay_cfg.relay_url, relay_cfg.room_id, conn_id
    );
    let answer_body = serde_json::json!({ "sdp": sdp_answer });
    if let Err(e) = state
        .http_client
        .post(&answer_url)
        .header("Authorization", format!("Bearer {}", relay_cfg.jwt))
        .json(&answer_body)
        .send()
        .await
    {
        tracing::error!("relay: failed to POST answer: {e}");
        return;
    }

    // Exchange ICE candidates via polling
    tokio::spawn(relay_ice_exchange(
        state.http_client.clone(),
        relay_cfg,
        conn_id,
        pc,
        server_candidates,
    ));
}

// ---------------------------------------------------------------------------
// ICE candidate exchange via relay HTTP polling
// ---------------------------------------------------------------------------

async fn relay_ice_exchange(
    http_client: reqwest::Client,
    relay_cfg: RelayConfig,
    conn_id: String,
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    server_candidates: Arc<tokio::sync::Mutex<Vec<RTCIceCandidateInit>>>,
) {
    let auth = format!("Bearer {}", relay_cfg.jwt);
    let base = &relay_cfg.relay_url;
    let room = &relay_cfg.room_id;

    // Poll for client ICE candidates and POST server ICE candidates
    // Run for up to 30 seconds (ICE should complete well within this)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        if tokio::time::Instant::now() > deadline {
            tracing::info!("relay: ICE exchange timeout for conn_id={conn_id}");
            break;
        }

        // POST server candidates
        {
            let mut cands = server_candidates.lock().await;
            if !cands.is_empty() {
                let candidates: Vec<serde_json::Value> = cands
                    .drain(..)
                    .map(|c| serde_json::json!({ "candidate": c.candidate, "sdpMid": c.sdp_mid, "sdpMLineIndex": c.sdp_mline_index }))
                    .collect();

                let url = format!("{base}/room/{room}/ice/{conn_id}/server");
                let _ = http_client
                    .post(&url)
                    .header("Authorization", &auth)
                    .json(&serde_json::json!({ "candidates": candidates }))
                    .send()
                    .await;
            }
        }

        // GET client candidates
        {
            let url = format!("{base}/room/{room}/ice/{conn_id}/client");
            if let Ok(resp) = http_client
                .get(&url)
                .header("Authorization", &auth)
                .send()
                .await
            {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(candidates) = body["candidates"].as_array() {
                        for c in candidates {
                            if let Some(candidate_str) = c.as_str() {
                                let init = RTCIceCandidateInit {
                                    candidate: candidate_str.to_string(),
                                    ..Default::default()
                                };
                                let _ = pc.add_ice_candidate(init).await;
                            }
                        }
                    }
                }
            }
        }

        // Check if connection is established
        let conn_state = pc.connection_state();
        if conn_state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {
            tracing::info!("relay: ICE connected for conn_id={conn_id}");
            break;
        }
        if conn_state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed
            || conn_state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed
        {
            tracing::warn!("relay: ICE failed/closed for conn_id={conn_id}");
            break;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
