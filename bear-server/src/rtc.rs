// ---------------------------------------------------------------------------
// WebRTC DataChannel transport — signaling via HTTP, data via SCTP/DTLS
// ---------------------------------------------------------------------------
//
// Flow:
//   1. Client creates RTCPeerConnection, creates a DataChannel "bear",
//      generates an SDP offer, POSTs it to /rtc/:session_id/offer.
//   2. Server creates its own RTCPeerConnection, sets the remote offer,
//      generates an SDP answer, returns it.
//   3. Both sides exchange ICE candidates via POST /rtc/:session_id/ice
//      (client→server) and POST /rtc/:session_id/candidates (server→client).
//   4. Once the DataChannel opens, the server relays JSON messages between
//      the DataChannel and the session bus — identical to the WebSocket path.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use bear_core::{ClientMessage, ServerMessage};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, RwLock};
use uuid::Uuid;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::state::ServerState;
use crate::ws::{ensure_worker_running, slash_command_infos};

// ---------------------------------------------------------------------------
// Shared state for active WebRTC peer connections
// ---------------------------------------------------------------------------

/// Holds a peer connection and its buffered ICE candidates (server-side).
pub(crate) struct PeerState {
    pc: Arc<RTCPeerConnection>,
    /// Server ICE candidates waiting to be polled by the client.
    pending_candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
}

/// Global map of active RTC peer connections, keyed by a connection ID.
pub type RtcPeers = Arc<RwLock<HashMap<String, PeerState>>>;

pub fn new_rtc_peers() -> RtcPeers {
    Arc::new(RwLock::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Signaling types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OfferRequest {
    pub sdp: String,
}

#[derive(Debug, Serialize)]
pub struct OfferResponse {
    pub conn_id: String,
    pub sdp: String,
}

#[derive(Debug, Deserialize)]
pub struct IceCandidateRequest {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(default)]
    pub sdp_mline_index: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct IceCandidateResponse {
    pub candidates: Vec<IceCandidateOut>,
}

#[derive(Debug, Serialize)]
pub struct IceCandidateOut {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

// ---------------------------------------------------------------------------
// POST /rtc/:session_id/offer — exchange SDP offer/answer
// ---------------------------------------------------------------------------

pub async fn rtc_offer(
    State(state): State<ServerState>,
    Path(session_id): Path<Uuid>,
    Json(payload): Json<OfferRequest>,
) -> impl IntoResponse {
    // Verify session exists
    let session_info = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.clone())
    };
    let Some(info) = session_info else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };

    // Build WebRTC API
    let mut media_engine = MediaEngine::default();
    if let Err(e) = media_engine.register_default_codecs() {
        tracing::error!("rtc: failed to register codecs: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "codec init failed").into_response();
    }

    let mut registry = Registry::new();
    registry = match register_default_interceptors(registry, &mut media_engine) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("rtc: interceptor init failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "interceptor init failed").into_response();
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
            tracing::error!("rtc: failed to create peer connection: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "peer connection failed").into_response();
        }
    };

    let conn_id = Uuid::new_v4().to_string();
    let pending_candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>> =
        Arc::new(Mutex::new(Vec::new()));

    // Collect server ICE candidates
    let cands = pending_candidates.clone();
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

    // When the remote peer creates a DataChannel named "bear", start relaying
    let relay_state = state.clone();
    let relay_session_id = session_id;
    let relay_info = info.clone();
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let state = relay_state.clone();
        let sid = relay_session_id;
        let info = relay_info.clone();
        Box::pin(async move {
            tracing::info!("rtc: data channel '{}' opened for session {sid}", dc.label());
            tokio::spawn(async move {
                handle_data_channel(state, sid, info, dc).await;
            });
        })
    }));

    // Set remote description (client's offer)
    let offer = RTCSessionDescription::offer(payload.sdp).unwrap();
    if let Err(e) = pc.set_remote_description(offer).await {
        tracing::error!("rtc: set_remote_description failed: {e}");
        return (StatusCode::BAD_REQUEST, "invalid offer").into_response();
    }

    // Create answer
    let answer = match pc.create_answer(None).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("rtc: create_answer failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "answer creation failed").into_response();
        }
    };

    // Set local description
    let sdp = answer.sdp.clone();
    if let Err(e) = pc.set_local_description(answer).await {
        tracing::error!("rtc: set_local_description failed: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "set local description failed",
        )
            .into_response();
    }

    // Store peer state
    {
        let mut peers = state.rtc_peers.write().await;
        peers.insert(
            conn_id.clone(),
            PeerState {
                pc,
                pending_candidates,
            },
        );
    }

    Json(OfferResponse {
        conn_id,
        sdp,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /rtc/:session_id/ice/:conn_id — client sends ICE candidate to server
// ---------------------------------------------------------------------------

pub async fn rtc_add_ice(
    State(state): State<ServerState>,
    Path((_session_id, conn_id)): Path<(Uuid, String)>,
    Json(payload): Json<IceCandidateRequest>,
) -> impl IntoResponse {
    let peers = state.rtc_peers.read().await;
    let Some(peer) = peers.get(&conn_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let init = RTCIceCandidateInit {
        candidate: payload.candidate,
        sdp_mid: payload.sdp_mid,
        sdp_mline_index: payload.sdp_mline_index,
        username_fragment: None,
    };

    if let Err(e) = peer.pc.add_ice_candidate(init).await {
        tracing::warn!("rtc: add_ice_candidate failed: {e}");
        return (StatusCode::BAD_REQUEST, "invalid candidate").into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// POST /rtc/:session_id/candidates/:conn_id — client polls server candidates
// ---------------------------------------------------------------------------

pub async fn rtc_get_candidates(
    State(state): State<ServerState>,
    Path((_session_id, conn_id)): Path<(Uuid, String)>,
) -> impl IntoResponse {
    let peers = state.rtc_peers.read().await;
    let Some(peer) = peers.get(&conn_id) else {
        return (StatusCode::NOT_FOUND, Json(IceCandidateResponse { candidates: vec![] })).into_response();
    };

    let mut cands = peer.pending_candidates.lock().await;
    let out: Vec<IceCandidateOut> = cands
        .drain(..)
        .map(|c| IceCandidateOut {
            candidate: c.candidate,
            sdp_mid: c.sdp_mid,
            sdp_mline_index: c.sdp_mline_index,
        })
        .collect();

    Json(IceCandidateResponse { candidates: out }).into_response()
}

// ---------------------------------------------------------------------------
// DataChannel relay — mirrors handle_socket logic from ws.rs
// ---------------------------------------------------------------------------

async fn handle_data_channel(
    state: ServerState,
    session_id: Uuid,
    info: bear_core::SessionInfo,
    dc: Arc<RTCDataChannel>,
) {
    // Send initial messages
    let _ = dc_send_msg(&dc, &ServerMessage::SessionInfo { session: info.clone() }).await;
    let _ = dc_send_msg(&dc, &ServerMessage::SlashCommands { commands: slash_command_infos() }).await;

    // Send shared client state (input history)
    {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&session_id) {
            let _ = dc_send_msg(&dc, &ServerMessage::ClientState {
                input_history: session.input_history.clone(),
            }).await;
        }
    }

    let _ = dc_send_msg(&dc, &ServerMessage::Notice {
        text: format!(
            "Session persists after clients disconnect. Working directory is {}.",
            info.cwd
        ),
    }).await;

    if info.name.is_none() {
        let _ = dc_send_msg(&dc, &ServerMessage::Notice {
            text: "Tip: Name this session with /session name <name>".to_string(),
        }).await;
    }

    // Ensure the session worker is running
    let client_tx = ensure_worker_running(&state, session_id).await;

    // Create a consumer for this client (starts at offset 0 — full replay)
    let mut consumer = {
        let buses = state.buses.read().await;
        let Some(bus) = buses.get(&session_id) else {
            let _ = dc_send_msg(&dc, &ServerMessage::Error {
                text: "session bus not found".to_string(),
            }).await;
            return;
        };
        bus.consumer()
    };

    tracing::info!("rtc: client connected to session {session_id}");

    // Channel for incoming DataChannel messages
    let (dc_msg_tx, mut dc_msg_rx) = mpsc::channel::<String>(64);

    // Register on_message handler
    let tx = dc_msg_tx.clone();
    dc.on_message(Box::new(move |msg| {
        let tx = tx.clone();
        let text = String::from_utf8_lossy(&msg.data).to_string();
        Box::pin(async move {
            let _ = tx.send(text).await;
        })
    }));

    // Register on_close handler
    let (close_tx, mut close_rx) = mpsc::channel::<()>(1);
    dc.on_close(Box::new(move || {
        let close_tx = close_tx.clone();
        Box::pin(async move {
            let _ = close_tx.send(()).await;
        })
    }));

    // Main relay loop
    loop {
        tokio::select! {
            // Messages from session worker → forward to DataChannel
            batch = consumer.next_batch() => {
                let mut send_failed = false;
                for msg in batch {
                    if dc_send_msg(&dc, &msg).await.is_err() {
                        tracing::info!("rtc: client disconnected from session {session_id} (send failed)");
                        send_failed = true;
                        break;
                    }
                }
                if send_failed { break; }
            }
            // Messages from DataChannel → forward to session worker
            dc_text = dc_msg_rx.recv() => {
                match dc_text {
                    Some(text) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Ping) => {
                                let _ = dc_send_msg(&dc, &ServerMessage::Pong).await;
                            }
                            Ok(client_msg) => {
                                let _ = client_tx.send(client_msg).await;
                            }
                            Err(err) => {
                                let _ = dc_send_msg(&dc, &ServerMessage::Error {
                                    text: format!("invalid message: {err}"),
                                }).await;
                            }
                        }
                    }
                    None => {
                        tracing::info!("rtc: dc_msg channel closed for session {session_id}");
                        break;
                    }
                }
            }
            // DataChannel closed
            _ = close_rx.recv() => {
                tracing::info!("rtc: DataChannel closed for session {session_id}");
                break;
            }
        }
    }

    tracing::info!("rtc: DataChannel relay ended for session {session_id}, worker continues");
}

/// Send a ServerMessage as JSON text over a DataChannel.
async fn dc_send_msg(dc: &Arc<RTCDataChannel>, msg: &ServerMessage) -> Result<(), String> {
    let payload = serde_json::to_string(msg).map_err(|e| e.to_string())?;
    dc.send_text(payload)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}
