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
use sha2::{Digest, Sha256};
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
        // Subscribe first so there is at least one receiver, then send.
        // watch::Sender::send() is a no-op when there are zero receivers,
        // and the initial receiver is dropped in new().
        let cmd_rx = self.cmd_tx.subscribe();
        let _ = self.cmd_tx.send(true);
        let handle_arc = self.handle.clone();
        tokio::spawn(async move {
            let mut handle = handle_arc.lock().await;
            if handle.as_ref().map_or(false, |h| !h.is_finished()) {
                return;
            }
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

    /// Notify the relay that this server is going offline.
    /// Called during graceful shutdown so the relay can immediately
    /// mark the room as offline instead of waiting for poll timeout.
    pub async fn notify_offline(http_client: &reqwest::Client) {
        let relay_cfg = match RelayConfig::load() {
            Some(c) => c,
            None => return,
        };
        let url = format!("{}/room/{}/status", relay_cfg.relay_url, relay_cfg.room_id);
        let auth = format!("Bearer {}", relay_cfg.jwt);
        match http_client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "online": false }))
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => {
                tracing::info!("relay: notified offline (status={})", resp.status());
            }
            Err(e) => {
                tracing::warn!("relay: failed to notify offline: {e}");
            }
        }
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
            broadcast_all_sessions(
                &state,
                relay_status("disabled", Some("disabled in config".into())),
            )
            .await;
            break;
        }

        // Build HTTP client: use pinned client if relay_tls_pin is set
        let http_client = if let Some(ref pin) = relay_cfg.relay_tls_pin {
            match crate::tls_pin::build_pinned_client(pin) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("relay: failed to build pinned HTTP client: {e}");
                    broadcast_all_sessions(
                        &state,
                        relay_status("error", Some(format!("TLS pin setup failed: {e}"))),
                    )
                    .await;
                    break;
                }
            }
        } else {
            state.http_client.clone()
        };

        // Poll for offers
        let poll_url = format!("{}/room/{}/offer", relay_cfg.relay_url, relay_cfg.room_id);
        let result = http_client
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
                let err_str = format!("{e}");

                // Detect TLS SPKI pin mismatch — fatal disconnect
                if err_str.contains("SPKI pin mismatch") {
                    tracing::error!("relay: TLS certificate pin mismatch — disconnecting");
                    broadcast_all_sessions(
                        &state,
                        relay_status(
                            "pin_mismatch",
                            Some(
                                "The relay server's TLS certificate has changed. \
                                 This could indicate a security issue. \
                                 Re-pair with --relay-pair to accept the new certificate, \
                                 or contact the relay operator."
                                    .into(),
                            ),
                        ),
                    )
                    .await;
                    break;
                }

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
    let secs = base
        .as_secs()
        .saturating_mul(1u64.checked_shl(count.min(10)).unwrap_or(u64::MAX));
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
// SAS verification code from DTLS fingerprints
// ---------------------------------------------------------------------------

fn extract_fingerprint(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("a=fingerprint:sha-256 ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn compute_sas(offer_sdp: &str, answer_sdp: &str) -> Option<String> {
    let fp_offer = extract_fingerprint(offer_sdp)?;
    let fp_answer = extract_fingerprint(answer_sdp)?;
    let mut fps = [fp_offer, fp_answer];
    fps.sort();
    let input = format!("{}:{}", fps[0], fps[1]);
    let hash = Sha256::digest(input.as_bytes());
    // First 3 bytes → 6 hex chars, uppercase
    Some(
        hash.iter()
            .take(3)
            .map(|b| format!("{b:02X}"))
            .collect::<String>(),
    )
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

    // Ensure a session worker is running (creates bus + worker on demand)
    let _ = crate::ws::ensure_worker_running(&state, session_id).await;

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
                if let Ok(mut init) = c.to_json() {
                    // The webrtc crate leaves sdp_mid empty; set it to "0"
                    // so the browser can associate candidates with the
                    // data channel m-line (a=mid:0 in the SDP).
                    init.sdp_mid = Some("0".to_string());
                    init.sdp_mline_index = Some(0);
                    cands.lock().await.push(init);
                }
            }
        })
    }));

    // When the remote peer creates a DataChannel, start relaying
    let relay_state = state.clone();
    let relay_info = info.clone();
    let notify_conn_id = conn_id.clone();
    pc.on_data_channel(Box::new(move |dc| {
        let state = relay_state.clone();
        let sid = session_id;
        let info = relay_info.clone();
        let cid = notify_conn_id.clone();
        Box::pin(async move {
            tracing::info!(
                "relay: data channel '{}' opened for session {sid}",
                dc.label()
            );
            broadcast_all_sessions(
                &state,
                ServerMessage::Notice {
                    text: format!("New remote browser connected ({})", cid),
                },
            )
            .await;
            tokio::spawn(async move {
                crate::rtc::handle_relay_data_channel(state, sid, info, dc).await;
            });
        })
    }));

    // Set remote description (browser's offer)
    let offer = RTCSessionDescription::offer(sdp_offer.clone()).unwrap();
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

    // Compute and broadcast SAS verification code
    if let Some(sas) = compute_sas(&sdp_offer, &sdp_answer) {
        tracing::info!("relay: verification code {sas} (conn_id={conn_id})");
        broadcast_all_sessions(
            &state,
            ServerMessage::Notice {
                text: format!("Verification: {sas} (conn_id={conn_id})"),
            },
        )
        .await;
    }

    // Mint a short-lived client JWT (5 minutes) for the browser's ICE exchange
    let client_jwt = {
        use pkcs8::DecodePrivateKey;
        match rsa::RsaPrivateKey::from_pkcs8_pem(&relay_cfg.private_key_pem) {
            Ok(pk) => match crate::mint_rs256_jwt(&pk, &relay_cfg.room_id, Some(300)) {
                Ok(jwt) => Some(jwt),
                Err(e) => {
                    tracing::warn!("relay: failed to mint client JWT: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("relay: failed to parse private key for client JWT: {e}");
                None
            }
        }
    };

    // POST answer to relay
    let answer_url = format!(
        "{}/room/{}/answer/{}",
        relay_cfg.relay_url, relay_cfg.room_id, conn_id
    );
    let mut answer_body = serde_json::json!({ "sdp": sdp_answer });
    if let Some(ref cjwt) = client_jwt {
        answer_body["client_jwt"] = serde_json::json!(cjwt);
    }
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
                    .map(|c| {
                        serde_json::json!({
                            "candidate": c.candidate,
                            "sdpMid": c.sdp_mid,
                            "sdpMLineIndex": c.sdp_mline_index,
                        })
                    })
                    .collect();

                let url = format!("{base}/room/{room}/ice/{conn_id}/server");
                for c in &candidates {
                    tracing::debug!("relay: ICE server candidate: {c}");
                }
                tracing::debug!(
                    "relay: ICE POST {count} server candidates to {url}",
                    count = candidates.len()
                );
                match http_client
                    .post(&url)
                    .header("Authorization", &auth)
                    .json(&serde_json::json!({ "candidates": candidates }))
                    .send()
                    .await
                {
                    Ok(resp) => tracing::debug!(
                        "relay: ICE POST server candidates status={}",
                        resp.status()
                    ),
                    Err(e) => tracing::warn!("relay: ICE POST server candidates failed: {e}"),
                }
            }
        }

        // GET client candidates
        {
            let url = format!("{base}/room/{room}/ice/{conn_id}/client");
            match http_client
                .get(&url)
                .header("Authorization", &auth)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(candidates) = body["candidates"].as_array() {
                            if !candidates.is_empty() {
                                tracing::debug!(
                                    "relay: ICE GET {count} client candidates (status={status})",
                                    count = candidates.len()
                                );
                            }
                            for c in candidates {
                                let (candidate_str, sdp_mid, sdp_mline_index) = if c.is_string() {
                                    (c.as_str().unwrap().to_string(), None, None)
                                } else if c.is_object() {
                                    (
                                        c["candidate"].as_str().unwrap_or("").to_string(),
                                        c["sdpMid"].as_str().map(|s| s.to_string()),
                                        c["sdpMLineIndex"].as_u64().map(|n| n as u16),
                                    )
                                } else {
                                    tracing::warn!(
                                        "relay: ICE client candidate unexpected type: {c}"
                                    );
                                    continue;
                                };
                                if candidate_str.is_empty() {
                                    continue;
                                }
                                tracing::debug!("relay: ICE adding client candidate: {candidate_str} sdpMid={sdp_mid:?} idx={sdp_mline_index:?}");
                                let init = RTCIceCandidateInit {
                                    candidate: candidate_str,
                                    sdp_mid: Some(sdp_mid.unwrap_or_default()),
                                    sdp_mline_index: Some(sdp_mline_index.unwrap_or(0)),
                                    username_fragment: None,
                                };
                                let _ = pc.add_ice_candidate(init).await;
                            }
                        } else {
                            tracing::debug!(
                                "relay: ICE GET client response (status={status}): {body}"
                            );
                        }
                    }
                }
                Err(e) => tracing::warn!("relay: ICE GET client candidates failed: {e}"),
            }
        }

        // Check if connection is established
        let conn_state = pc.connection_state();
        if conn_state
            == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected
        {
            tracing::info!("relay: ICE connected for conn_id={conn_id}");
            break;
        }
        if conn_state
            == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed
            || conn_state
                == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed
        {
            tracing::warn!("relay: ICE failed/closed for conn_id={conn_id}");
            break;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
