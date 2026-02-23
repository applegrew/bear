use anyhow::{Context, Result};
use bear_core::DEFAULT_SERVER_URL;
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_DIR: &str = "/tmp/bear";
const LOG_FILE: &str = "/tmp/bear/server.log";

/// Ensure `bear-server` is running. Probe first; if not reachable, launch it.
pub async fn ensure_server_running() -> Result<()> {
    if probe_server(PROBE_TIMEOUT).await {
        return Ok(());
    }

    eprintln!("  Starting bear-server...");
    launch_server().context("failed to launch bear-server")?;

    // Poll until the server responds or we time out
    let start = Instant::now();
    loop {
        tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;

        if probe_server(PROBE_TIMEOUT).await {
            eprintln!("  bear-server is ready.");
            return Ok(());
        }

        if start.elapsed() > LAUNCH_TIMEOUT {
            // Try to show the user what went wrong
            let log_hint = if Path::new(LOG_FILE).exists() {
                match fs::read_to_string(LOG_FILE) {
                    Ok(contents) => {
                        let tail: String = contents.lines().rev().take(20).collect::<Vec<_>>()
                            .into_iter().rev().collect::<Vec<_>>().join("\n");
                        format!("\n  Last log lines from {LOG_FILE}:\n{tail}")
                    }
                    Err(_) => format!("\n  Check {LOG_FILE} for details."),
                }
            } else {
                String::new()
            };
            anyhow::bail!(
                "bear-server started but is not responding at {DEFAULT_SERVER_URL} after {}s.{log_hint}",
                LAUNCH_TIMEOUT.as_secs()
            );
        }
    }
}

/// Try to reach the server with a short HTTP GET to `/sessions`.
async fn probe_server(timeout: Duration) -> bool {
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build();
    let Ok(client) = client else { return false };
    let url = format!("{DEFAULT_SERVER_URL}/sessions");
    client.get(&url).send().await.is_ok()
}

/// Spawn `bear-server` as a detached background process with logs to `/tmp/bear/server.log`.
fn launch_server() -> Result<()> {
    fs::create_dir_all(LOG_DIR)
        .with_context(|| format!("failed to create log directory {LOG_DIR}"))?;

    let log_file: File = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(LOG_FILE)
        .with_context(|| format!("failed to open {LOG_FILE}"))?;

    let stderr_file = log_file.try_clone()
        .context("failed to clone log file handle")?;

    let mut cmd = Command::new("bear-server");
    cmd.stdout(log_file).stderr(stderr_file);

    // On Unix, start a new session so the server survives client exit
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn().context("failed to spawn bear-server (is it on your PATH?)")?;

    Ok(())
}
