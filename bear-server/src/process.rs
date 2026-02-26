use bear_core::ServerMessage;
use uuid::Uuid;

use crate::state::{BusSender, ServerState};

// ---------------------------------------------------------------------------
// Process management helpers
// ---------------------------------------------------------------------------

/// Send SIGTERM to a process, wait briefly, then SIGKILL if still alive.
fn kill_process(pid: u32) {
    #[cfg(unix)]
    {
        use std::time::Duration;
        unsafe {
            // SIGTERM first for graceful shutdown
            libc::kill(pid as i32, libc::SIGTERM);
        }
        // Give it a moment to exit gracefully
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            // Check if still alive (signal 0 = existence check)
            if libc::kill(pid as i32, 0) == 0 {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Fallback for non-unix: use taskkill or similar
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
    }
}

pub async fn handle_process_kill(state: &ServerState, bus: &BusSender, pid: u32) {
    let exists = state.processes.read().await.contains_key(&pid);
    if !exists {
        bus.send(ServerMessage::Error {
            text: format!("No managed process with pid {pid}"),
        })
        .await;
        return;
    }

    kill_process(pid);

    let mut procs = state.processes.write().await;
    if let Some(p) = procs.get_mut(&pid) {
        p.info.running = false;
        p.stdin_tx = None;
    }
    bus.send(ServerMessage::ProcessExited { pid, code: None })
        .await;
}

pub async fn handle_process_input(state: &ServerState, bus: &BusSender, pid: u32, text: &str) {
    let procs = state.processes.read().await;
    if let Some(p) = procs.get(&pid) {
        if let Some(tx) = &p.stdin_tx {
            let _ = tx.send(text.to_string()).await;
        } else {
            bus.send(ServerMessage::Error {
                text: format!("Process {pid} stdin closed"),
            })
            .await;
        }
    } else {
        bus.send(ServerMessage::Error {
            text: format!("No managed process with pid {pid}"),
        })
        .await;
    }
}

/// Clean up processes belonging to a session when the WebSocket disconnects.
/// Marks them as not running and drops stdin senders (which will cause
/// the stdin forwarding tasks to exit).
pub async fn cleanup_session_processes(state: &ServerState, session_id: Uuid) {
    let mut procs = state.processes.write().await;
    for p in procs.values_mut() {
        if p.session_id == session_id && p.info.running {
            p.info.running = false;
            p.stdin_tx = None;
        }
    }
    // Remove stale entries for this session
    procs.retain(|_, p| p.session_id != session_id || p.info.running);
}
