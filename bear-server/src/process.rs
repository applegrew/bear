use axum::extract::ws::WebSocket;
use bear_core::ServerMessage;
use std::process::Command as StdCommand;

use crate::state::ServerState;
use crate::ws::send_msg;

// ---------------------------------------------------------------------------
// Process management helpers
// ---------------------------------------------------------------------------

pub async fn handle_process_kill(
    state: &ServerState,
    socket: &mut WebSocket,
    pid: u32,
) {
    let exists = state.processes.read().await.contains_key(&pid);
    if !exists {
        let _ = send_msg(socket, ServerMessage::Error {
            text: format!("No managed process with pid {pid}"),
        }).await;
        return;
    }

    let _ = StdCommand::new("kill").arg(pid.to_string()).status();
    let mut procs = state.processes.write().await;
    if let Some(p) = procs.get_mut(&pid) {
        p.info.running = false;
        p.stdin_tx = None;
    }
    let _ = send_msg(socket, ServerMessage::ProcessExited { pid, code: None }).await;
}

pub async fn handle_process_input(
    state: &ServerState,
    socket: &mut WebSocket,
    pid: u32,
    text: &str,
) {
    let procs = state.processes.read().await;
    if let Some(p) = procs.get(&pid) {
        if let Some(tx) = &p.stdin_tx {
            let _ = tx.send(text.to_string()).await;
        } else {
            let _ = send_msg(socket, ServerMessage::Error {
                text: format!("Process {pid} stdin closed"),
            }).await;
        }
    } else {
        let _ = send_msg(socket, ServerMessage::Error {
            text: format!("No managed process with pid {pid}"),
        }).await;
    }
}
