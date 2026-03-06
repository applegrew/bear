// ---------------------------------------------------------------------------
// WebRTC DataChannel transport — signaling via relay, data via SCTP/DTLS
// ---------------------------------------------------------------------------
//
// Flow:
//   1. Browser client connects via relay signaling (SDP offer/answer + ICE
//      exchange proxied through the public server and relay).
//   2. Once the DataChannel opens, the client enters a "lobby" state where
//      it can list, create, or select a session via DataChannel messages.
//   3. After session selection, the DataChannel relay loop mirrors the
//      WebSocket path — forwarding messages between client and session bus.

use std::sync::Arc;

use bear_core::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use uuid::Uuid;
use webrtc::data_channel::RTCDataChannel;

use crate::state::ServerState;
use crate::ws::{ensure_worker_running, slash_command_infos};

// ---------------------------------------------------------------------------
// DataChannel relay — mirrors handle_socket logic from ws.rs
// ---------------------------------------------------------------------------

/// Public entry point for relay module to hand off a DataChannel.
/// Starts in "lobby" state — no session is bound yet. The client must
/// send SessionList / SessionCreate / SessionSelect messages to pick a
/// session before normal relay begins.
pub async fn handle_relay_data_channel(state: ServerState, dc: Arc<RTCDataChannel>) {
    handle_data_channel_lobby(state, dc).await;
}

/// Lobby state: DataChannel is open but not bound to any session.
/// Handles session management messages, then transitions to the bound relay loop.
async fn handle_data_channel_lobby(state: ServerState, dc: Arc<RTCDataChannel>) {
    // Register message/close handlers FIRST to avoid a race condition:
    // the client sends session_list immediately on dc.onopen, so we must
    // be listening before any async work (like sending SlashCommands).
    let (dc_msg_tx, mut dc_msg_rx) = mpsc::channel::<String>(64);
    let tx = dc_msg_tx.clone();
    dc.on_message(Box::new(move |msg| {
        let tx = tx.clone();
        let text = String::from_utf8_lossy(&msg.data).to_string();
        Box::pin(async move {
            let _ = tx.send(text).await;
        })
    }));

    let (close_tx, mut close_rx) = mpsc::channel::<()>(1);
    dc.on_close(Box::new(move || {
        let close_tx = close_tx.clone();
        Box::pin(async move {
            let _ = close_tx.send(()).await;
        })
    }));

    // Send slash commands (available before session binding)
    let _ = dc_send_msg(
        &dc,
        &ServerMessage::SlashCommands {
            commands: slash_command_infos(),
        },
    )
    .await;

    tracing::info!("rtc: lobby — waiting for session selection");

    // Lobby loop: wait for session management messages
    loop {
        tokio::select! {
            dc_text = dc_msg_rx.recv() => {
                match dc_text {
                    Some(text) => {
                        let client_msg = match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(m) => m,
                            Err(err) => {
                                let _ = dc_send_msg(&dc, &ServerMessage::Error {
                                    text: format!("invalid message: {err}"),
                                }).await;
                                continue;
                            }
                        };

                        match client_msg {
                            ClientMessage::Ping => {
                                let _ = dc_send_msg(&dc, &ServerMessage::Pong).await;
                            }
                            ClientMessage::SessionList => {
                                let sessions = state.sessions.read().await;
                                let items: Vec<bear_core::SessionInfo> = sessions
                                    .values()
                                    .map(|s| s.info.clone())
                                    .collect();
                                let _ = dc_send_msg(
                                    &dc,
                                    &ServerMessage::SessionListResult { sessions: items },
                                ).await;
                            }
                            ClientMessage::SessionCreate { cwd } => {
                                let info = crate::do_create_session(&state, cwd).await;
                                let _ = dc_send_msg(
                                    &dc,
                                    &ServerMessage::SessionCreated { session: info.clone() },
                                ).await;
                                // Auto-bind to the newly created session
                                tracing::info!("rtc: lobby — created and binding to session {}", info.id);
                                handle_data_channel_bound(state, info.id, info, dc, dc_msg_rx, close_rx, false).await;
                                return;
                            }
                            ClientMessage::SessionSelect { session_id, reconnect } => {
                                let session_info = {
                                    let sessions = state.sessions.read().await;
                                    sessions.get(&session_id).map(|s| s.info.clone())
                                };
                                match session_info {
                                    Some(info) => {
                                        tracing::info!("rtc: lobby — binding to session {session_id} (reconnect={reconnect})");
                                        handle_data_channel_bound(state, session_id, info, dc, dc_msg_rx, close_rx, reconnect).await;
                                        return;
                                    }
                                    None => {
                                        let _ = dc_send_msg(&dc, &ServerMessage::Error {
                                            text: format!("session {session_id} not found"),
                                        }).await;
                                    }
                                }
                            }
                            _ => {
                                let _ = dc_send_msg(&dc, &ServerMessage::Error {
                                    text: "select a session first (send session_list, session_create, or session_select)".to_string(),
                                }).await;
                            }
                        }
                    }
                    None => {
                        tracing::info!("rtc: lobby — dc_msg channel closed");
                        return;
                    }
                }
            }
            _ = close_rx.recv() => {
                tracing::info!("rtc: lobby — DataChannel closed");
                return;
            }
        }
    }
}

/// Bound state: DataChannel is bound to a specific session.
/// This is the post-lobby relay loop — identical to the original handle_data_channel
/// but accepts pre-created message channels.
async fn handle_data_channel_bound(
    state: ServerState,
    session_id: Uuid,
    info: bear_core::SessionInfo,
    dc: Arc<RTCDataChannel>,
    mut dc_msg_rx: mpsc::Receiver<String>,
    mut close_rx: mpsc::Receiver<()>,
    reconnect: bool,
) {
    // Send initial messages (same as WS flow after session selection)
    let _ = dc_send_msg(
        &dc,
        &ServerMessage::SessionInfo {
            session: info.clone(),
        },
    )
    .await;

    // Send shared client state (input history)
    {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&session_id) {
            let _ = dc_send_msg(
                &dc,
                &ServerMessage::ClientState {
                    input_history: session.input_history.clone(),
                },
            )
            .await;
        }
    }

    let _ = dc_send_msg(
        &dc,
        &ServerMessage::Notice {
            text: format!(
                "Session persists after clients disconnect. Working directory is {}.",
                info.cwd
            ),
        },
    )
    .await;

    if info.name.is_none() {
        let _ = dc_send_msg(
            &dc,
            &ServerMessage::Notice {
                text: "Tip: Name this session with /session name <name>".to_string(),
            },
        )
        .await;
    }

    // Ensure the session worker is running
    let client_tx = ensure_worker_running(&state, session_id).await;

    // Create a consumer — reconnect starts at end (no replay), fresh starts at 0
    let mut consumer = {
        let buses = state.buses.read().await;
        let Some(bus) = buses.get(&session_id) else {
            let _ = dc_send_msg(
                &dc,
                &ServerMessage::Error {
                    text: "session bus not found".to_string(),
                },
            )
            .await;
            return;
        };
        if reconnect {
            bus.consumer_at_end().await
        } else {
            bus.consumer()
        }
    };

    tracing::info!("rtc: client connected to session {session_id} (reconnect={reconnect})");

    let mut prompt_active = false;
    let mut scanned_len: usize = 0;

    // For fresh connects, wrap the initial drain with ReplayStart/ReplayEnd
    // so clients can suppress already-resolved prompts during history replay.
    if !reconnect {
        let replay_msgs = consumer.peek().await;
        let count = replay_msgs.len();
        if count > 0 {
            let _ = dc_send_msg(&dc, &ServerMessage::ReplayStart { count }).await;
            dc_drain_unconsumed(&mut consumer, &dc, &mut prompt_active).await;
            let _ = dc_send_msg(&dc, &ServerMessage::ReplayEnd).await;
            if prompt_active {
                scanned_len = consumer.offset();
            }
        }
    }

    // Main relay loop
    loop {
        if prompt_active {
            tokio::select! {
                _ = consumer.wait_changed(scanned_len) => {
                    let peeked = consumer.peek().await;
                    scanned_len = consumer.offset() + peeked.len();
                    if let Some(pos) = peeked.iter().position(|m| m.is_prompt_resolution()) {
                        for msg in &peeked[..=pos] {
                            let _ = dc_send_msg(&dc, msg).await;
                        }
                        consumer.advance(pos + 1);
                        prompt_active = false;
                        dc_drain_unconsumed(&mut consumer, &dc, &mut prompt_active).await;
                        if prompt_active {
                            scanned_len = consumer.offset();
                        }
                    }
                }
                dc_text = dc_msg_rx.recv() => {
                    match dc_text {
                        Some(text) => {
                            if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                                if matches!(client_msg, ClientMessage::Ping) {
                                    let _ = dc_send_msg(&dc, &ServerMessage::Pong).await;
                                    continue;
                                }
                                let resolves = matches!(
                                    client_msg,
                                    ClientMessage::ToolConfirm { .. }
                                        | ClientMessage::UserPromptResponse { .. }
                                        | ClientMessage::TaskPlanResponse { .. }
                                );
                                tracing::info!("rtc: forwarding {client_msg:?} to session {session_id}");
                                let _ = client_tx.try_send(client_msg);
                                if resolves {
                                    prompt_active = false;
                                    dc_drain_unconsumed(&mut consumer, &dc, &mut prompt_active).await;
                                    if prompt_active {
                                        scanned_len = consumer.offset();
                                    }
                                }
                            }
                        }
                        None => {
                            tracing::info!("rtc: dc_msg channel closed for session {session_id}");
                            break;
                        }
                    }
                }
                _ = close_rx.recv() => {
                    tracing::info!("rtc: DataChannel closed for session {session_id}");
                    break;
                }
            }
        } else {
            tokio::select! {
                _peeked = consumer.wait_peek() => {
                    dc_drain_unconsumed(&mut consumer, &dc, &mut prompt_active).await;
                    if prompt_active {
                        scanned_len = consumer.offset();
                    }
                }
                dc_text = dc_msg_rx.recv() => {
                    match dc_text {
                        Some(text) => {
                            match serde_json::from_str::<ClientMessage>(&text) {
                                Ok(ClientMessage::Ping) => {
                                    let _ = dc_send_msg(&dc, &ServerMessage::Pong).await;
                                }
                                Ok(client_msg) => {
                                    tracing::info!("rtc: forwarding {client_msg:?} to session {session_id}");
                                    match client_tx.try_send(client_msg) {
                                        Ok(()) => {}
                                        Err(mpsc::error::TrySendError::Full(msg)) => {
                                            tracing::warn!("rtc: client_tx full for session {session_id}, dropping {msg:?}");
                                            let _ = dc_send_msg(&dc, &ServerMessage::Error {
                                                text: "Server busy — please try again in a moment.".to_string(),
                                            }).await;
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => {
                                            tracing::warn!("rtc: client_tx closed for session {session_id}");
                                        }
                                    }
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
                _ = close_rx.recv() => {
                    tracing::info!("rtc: DataChannel closed for session {session_id}");
                    break;
                }
            }
        }
    }

    tracing::info!("rtc: DataChannel relay ended for session {session_id}, worker continues");
}

/// Peek unconsumed messages from the consumer and forward them to the
/// DataChannel, advancing the offset one-by-one.  Stops (and sets
/// `prompt_active = true`) as soon as an interactive prompt is forwarded,
/// leaving subsequent messages unconsumed in the topic log.
async fn dc_drain_unconsumed(
    consumer: &mut crate::state::TopicConsumer,
    dc: &Arc<RTCDataChannel>,
    prompt_active: &mut bool,
) {
    let peeked = consumer.peek().await;
    for msg in peeked.iter() {
        if msg.is_interactive_prompt() {
            *prompt_active = true;
        }
        let _ = dc_send_msg(dc, msg).await;
        consumer.advance(1);
        if *prompt_active {
            break;
        }
    }
}

/// Maximum payload size per DataChannel message.
/// SCTP (used by WebRTC DataChannels) typically limits messages to 16 KB.
/// We use 15 KB to leave headroom for the chunk envelope.
const DC_MAX_PAYLOAD: usize = 15_000;

/// Split `payload` into chunks of at most `max_bytes` each, respecting
/// UTF-8 char boundaries. Returns a vec of string slices.
fn chunk_payload(payload: &str, max_bytes: usize) -> Vec<&str> {
    assert!(max_bytes > 0, "max_bytes must be > 0");
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < payload.len() {
        let mut end = (start + max_bytes).min(payload.len());
        // Back up to a char boundary
        while end > start && !payload.is_char_boundary(end) {
            end -= 1;
        }
        // If we backed up all the way (char wider than max_bytes), include
        // at least one full char to guarantee forward progress.
        if end == start {
            end = start + payload[start..].chars().next().map_or(0, |c| c.len_utf8());
        }
        chunks.push(&payload[start..end]);
        start = end;
    }
    chunks
}

/// Send a ServerMessage as JSON text over a DataChannel.
/// If the serialized JSON exceeds `DC_MAX_PAYLOAD`, it is split into numbered
/// chunks that the browser client reassembles.
async fn dc_send_msg(dc: &Arc<RTCDataChannel>, msg: &ServerMessage) -> Result<(), String> {
    let payload = serde_json::to_string(msg).map_err(|e| e.to_string())?;

    if payload.len() <= DC_MAX_PAYLOAD {
        return dc
            .send_text(payload)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
    }

    let chunk_id = uuid::Uuid::new_v4().to_string();
    let chunks = chunk_payload(&payload, DC_MAX_PAYLOAD);
    let total = chunks.len();
    tracing::debug!(
        "dc_send_msg: splitting {}-byte payload into {total} chunks",
        payload.len()
    );

    for (idx, data) in chunks.iter().enumerate() {
        let envelope = serde_json::json!({
            "__chunk": true,
            "id": chunk_id,
            "idx": idx,
            "total": total,
            "data": data,
        });
        dc.send_text(envelope.to_string())
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- chunk_payload tests ------------------------------------------------

    #[test]
    fn chunk_small_payload_single_chunk() {
        let payload = "hello";
        let chunks = chunk_payload(payload, 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_exact_boundary() {
        let payload = "abcdef";
        let chunks = chunk_payload(payload, 3);
        assert_eq!(chunks, vec!["abc", "def"]);
    }

    #[test]
    fn chunk_uneven_split() {
        let payload = "abcdefg";
        let chunks = chunk_payload(payload, 3);
        assert_eq!(chunks, vec!["abc", "def", "g"]);
    }

    #[test]
    fn chunk_respects_utf8_boundaries() {
        // '€' is 3 bytes. "a€b" = 5 bytes total.
        let payload = "a€b";
        // max_bytes=4: "a€" (4 bytes) + "b" (1 byte)
        let chunks = chunk_payload(payload, 4);
        assert_eq!(chunks, vec!["a€", "b"]);
        assert_eq!(chunks.concat(), payload);
    }

    #[test]
    fn chunk_wide_char_exceeds_max_bytes() {
        // '€' is 3 bytes. With max_bytes=2, the chunker must still
        // make forward progress by emitting the full char.
        let payload = "€";
        let chunks = chunk_payload(payload, 2);
        assert_eq!(chunks, vec!["€"]);
    }

    #[test]
    fn chunk_reassembles_to_original() {
        let payload = "x".repeat(50_000);
        let chunks = chunk_payload(&payload, 15_000);
        assert_eq!(chunks.len(), 4); // 15000 + 15000 + 15000 + 5000
        let reassembled: String = chunks.concat();
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn chunk_large_json_payload() {
        // Simulate a large ToolOutput JSON
        let big_output = "a".repeat(40_000);
        let msg = serde_json::json!({
            "type": "tool_output",
            "output": big_output,
        });
        let payload = serde_json::to_string(&msg).unwrap();
        let chunks = chunk_payload(&payload, DC_MAX_PAYLOAD);
        assert!(chunks.len() >= 3);
        let reassembled: String = chunks.concat();
        assert_eq!(reassembled, payload);
        // Verify reassembled JSON is valid
        let parsed: serde_json::Value = serde_json::from_str(&reassembled).unwrap();
        assert_eq!(parsed["type"], "tool_output");
    }

    #[test]
    fn chunk_empty_payload() {
        let chunks = chunk_payload("", 100);
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_single_byte_max() {
        let payload = "abc";
        let chunks = chunk_payload(payload, 1);
        assert_eq!(chunks, vec!["a", "b", "c"]);
    }
}
