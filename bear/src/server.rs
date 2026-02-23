use anyhow::{Context, Result};
use bear_core::DEFAULT_SERVER_URL;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
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

/// Read the server PID from `~/.bear/server.pid`. Returns `None` if the file
/// doesn't exist or the PID is not a running process.
fn read_server_pid() -> Option<u32> {
    let path = bear_core::server_pid_path()?;
    let contents = fs::read_to_string(&path).ok()?;
    let pid: u32 = contents.trim().parse().ok()?;

    // Check if the process is actually alive
    #[cfg(unix)]
    {
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive {
            // Stale PID file — clean it up
            let _ = fs::remove_file(&path);
            return None;
        }
    }

    Some(pid)
}

/// Send SIGTERM to the server process and wait for it to exit.
fn kill_server_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }

        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                break;
            }
            if start.elapsed() > STOP_TIMEOUT {
                // Force kill
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                std::thread::sleep(Duration::from_millis(200));
                break;
            }
        }
    }

    // Clean up PID file if it still exists
    if let Some(path) = bear_core::server_pid_path() {
        let _ = fs::remove_file(&path);
    }

    Ok(())
}

/// Stop the running bear-server. Exits without launching a client session.
pub async fn stop_server() -> Result<()> {
    match read_server_pid() {
        Some(pid) => {
            eprintln!("  Stopping bear-server (pid {pid})...");
            kill_server_pid(pid)?;
            eprintln!("  bear-server stopped.");
            Ok(())
        }
        None => {
            // Also try probing — maybe PID file is missing but server is running
            if probe_server(PROBE_TIMEOUT).await {
                eprintln!("  bear-server is running but PID file is missing.");
                eprintln!("  Please stop it manually or use `kill` on the process.");
                anyhow::bail!("cannot stop server: PID file not found");
            }
            eprintln!("  bear-server is not running.");
            Ok(())
        }
    }
}

/// Restart the bear-server: stop if running, then launch fresh.
pub async fn restart_server() -> Result<()> {
    if let Some(pid) = read_server_pid() {
        eprintln!("  Stopping bear-server (pid {pid})...");
        kill_server_pid(pid)?;
    } else if probe_server(PROBE_TIMEOUT).await {
        eprintln!("  bear-server is running but PID file is missing.");
        eprintln!("  Please stop it manually first.");
        anyhow::bail!("cannot restart server: PID file not found");
    }

    eprintln!("  Starting bear-server...");
    launch_server().context("failed to launch bear-server")?;

    let start = Instant::now();
    loop {
        tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;
        if probe_server(PROBE_TIMEOUT).await {
            eprintln!("  bear-server is ready.");
            return Ok(());
        }
        if start.elapsed() > LAUNCH_TIMEOUT {
            anyhow::bail!(
                "bear-server started but is not responding at {DEFAULT_SERVER_URL} after {}s.",
                LAUNCH_TIMEOUT.as_secs()
            );
        }
    }
}

/// If the server is currently running, prompt the user to restart it.
/// Used by --disable-relay and --enable-relay.
pub async fn prompt_restart_if_running() -> Result<()> {
    if read_server_pid().is_none() && !probe_server(PROBE_TIMEOUT).await {
        eprintln!("  (server is not running — changes will take effect on next start)");
        return Ok(());
    }

    eprint!("  Server is running. Restart now for changes to take effect? [y/N] ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if input == "y" || input == "yes" {
        restart_server().await
    } else {
        eprintln!("  Changes will take effect on next server restart.");
        Ok(())
    }
}
