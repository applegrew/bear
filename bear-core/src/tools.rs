use std::io::BufRead;
use std::panic::AssertUnwindSafe;

use crate::{PendingToolCall, ProcessInfo, ServerMessage, TodoItem, UndoEntry};
use async_trait::async_trait;
use futures::FutureExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Tool call parsing from LLM output
// ---------------------------------------------------------------------------

pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    // Format 1: [TOOL_CALL]{"name": "tool", "arguments": {…}}[/TOOL_CALL]
    // Also handles malformed variant: [TOOL_CALL{"name": ...}[/TOOL_CALL] (missing ] after TOOL_CALL)
    {
        let mut pos = 0;
        let markers: &[&str] = &["[TOOL_CALL]", "[TOOL_CALL"];
        while pos < text.len() {
            // Find the earliest occurrence of either marker
            let mut best: Option<(usize, usize)> = None; // (abs_start, json_start)
            for marker in markers {
                if let Some(offset) = text[pos..].find(marker) {
                    let abs = pos + offset;
                    let js = abs + marker.len();
                    if best.is_none() || abs < best.unwrap().0 {
                        best = Some((abs, js));
                    }
                }
            }
            let Some((_abs_start, json_start)) = best else {
                break;
            };

            if let Some(end) = text[json_start..].find("[/TOOL_CALL]") {
                let json_str = &text[json_start..json_start + end];
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let (Some(name), Some(args)) = (val["name"].as_str(), val.get("arguments")) {
                        calls.push(ParsedToolCall {
                            name: name.to_string(),
                            arguments: args.clone(),
                        });
                    }
                }
                pos = json_start + end + "[/TOOL_CALL]".len();
            } else {
                break;
            }
        }
    }

    // Format 2: [tool_name]{args}[/tool_name]  (only if format 1 found nothing)
    if calls.is_empty() {
        let mut pos = 0;
        while pos < text.len() {
            // Find opening bracket
            let Some(bracket) = text[pos..].find('[') else {
                break;
            };
            let abs_bracket = pos + bracket;
            // Find closing bracket
            let after = &text[abs_bracket + 1..];
            let Some(close_bracket) = after.find(']') else {
                break;
            };
            let tag_name = &after[..close_bracket];

            // Must be snake_case with at least one underscore
            let is_tool = tag_name.contains('_')
                && tag_name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_');

            if !is_tool {
                pos = abs_bracket + 1 + close_bracket + 1;
                continue;
            }

            let json_start = abs_bracket + 1 + close_bracket + 1;
            let close_tag = format!("[/{tag_name}]");
            let Some(close_pos) = text[json_start..].find(&close_tag) else {
                pos = json_start;
                continue;
            };

            let json_str = &text[json_start..json_start + close_pos];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if val.is_object() {
                    calls.push(ParsedToolCall {
                        name: tag_name.to_string(),
                        arguments: val,
                    });
                }
            }
            pos = json_start + close_pos + close_tag.len();
        }
    }

    calls
}

// ---------------------------------------------------------------------------
// Tool execution context trait
// ---------------------------------------------------------------------------

/// Trait for server-side capabilities needed by tool execution.
/// Implemented by bear-server's ServerState.
#[async_trait]
pub trait ToolContext: Send + Sync {
    fn http_client(&self) -> &reqwest::Client;
    fn max_tool_output_chars(&self) -> usize;

    // Web search fallback keys
    fn google_api_key(&self) -> Option<&str>;
    fn google_cx(&self) -> Option<&str>;
    fn brave_api_key(&self) -> Option<&str>;

    // Session state access
    async fn get_session_cwd(&self, session_id: Uuid) -> Option<String>;
    async fn push_undo(&self, session_id: Uuid, path: &str, previous_content: String);
    async fn get_undo_entries(&self, session_id: Uuid, steps: usize) -> Vec<UndoEntry>;
    async fn set_todo_list(&self, session_id: Uuid, items: Vec<TodoItem>);
    async fn get_todo_list(&self, session_id: Uuid) -> Vec<TodoItem>;

    // Session workdir
    async fn set_session_cwd(&self, session_id: Uuid, new_cwd: String);

    // Process management
    async fn register_process(
        &self,
        session_id: Uuid,
        pid: u32,
        command: String,
        stdin_tx: mpsc::Sender<String>,
    );
    async fn mark_process_exited(&self, pid: u32);

    // Workspace (.bear/) persistence
    async fn load_workspace_auto_approved(&self, cwd: &str) -> std::collections::HashSet<String>;
    async fn save_workspace_auto_approved(
        &self,
        cwd: &str,
        set: &std::collections::HashSet<String>,
    );
    async fn reset_session_auto_approved(
        &self,
        session_id: Uuid,
        new_set: std::collections::HashSet<String>,
    );
    async fn save_script(
        &self,
        cwd: &str,
        script: &crate::workspace::SavedScript,
    ) -> Result<(), String>;
    async fn load_script(
        &self,
        cwd: &str,
        name: &str,
    ) -> Result<crate::workspace::SavedScript, String>;
    async fn list_scripts(&self, cwd: &str) -> Vec<crate::workspace::SavedScript>;
    async fn save_plan(
        &self,
        cwd: &str,
        plan: &crate::workspace::SavedPlan,
    ) -> Result<(), String>;
    async fn load_plan(
        &self,
        cwd: &str,
        name: &str,
    ) -> Result<crate::workspace::SavedPlan, String>;
    async fn list_plans(&self, cwd: &str) -> Vec<crate::workspace::SavedPlan>;
    async fn delete_plan(&self, cwd: &str, name: &str) -> Result<(), String>;

    // LSP access
    async fn lsp_diagnostics(
        &self,
        file_path: &str,
        workspace_root: &str,
    ) -> Result<String, String>;
    async fn lsp_hover(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<String, String>;
    async fn lsp_references(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<String, String>;
    async fn lsp_symbols(&self, file_path: &str, workspace_root: &str) -> Result<String, String>;
    async fn lsp_find_symbol_range(
        &self,
        file_path: &str,
        symbol: &str,
        workspace_root: &str,
    ) -> Result<(u32, u32), String>;
}

/// Trait for sending messages to connected clients (bus).
#[async_trait]
pub trait ToolBus: Send + Sync {
    async fn send(&self, msg: ServerMessage);
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

pub async fn execute_tool(
    ctx: &dyn ToolContext,
    bus: &dyn ToolBus,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    match AssertUnwindSafe(execute_tool_inner(ctx, bus, session_id, ptc))
        .catch_unwind()
        .await
    {
        Ok(output) => output,
        Err(panic) => {
            let msg = if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = panic.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown panic".to_string()
            };
            tracing::error!("tool '{}' panicked: {msg}", ptc.tool_call.name);
            format!("Error: tool panicked: {msg}")
        }
    }
}

async fn execute_tool_inner(
    ctx: &dyn ToolContext,
    bus: &dyn ToolBus,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    match ptc.tool_call.name.as_str() {
        "run_command" => {
            let cmd_str = ptc.tool_call.arguments["command"]
                .as_str()
                .unwrap_or("echo 'no command'")
                .to_string();
            execute_run_command(ctx, bus, session_id, &cmd_str, &ptc.cwd).await
        }
        "read_file" => {
            let path = ptc.tool_call.arguments["path"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let full_path = match validate_tool_path(&path, &ptc.cwd) {
                Ok(p) => p,
                Err(e) => return e,
            };
            const MAX_READ_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
            match tokio::fs::metadata(&full_path).await {
                Ok(meta) if meta.len() > MAX_READ_SIZE => {
                    return format!(
                        "Error: file is {} bytes ({:.1} MB) which exceeds the 10 MB limit. \
                         Use run_command with head/tail to read portions of large files.",
                        meta.len(),
                        meta.len() as f64 / (1024.0 * 1024.0),
                    );
                }
                Err(err) => return format!("Error reading {full_path}: {err}"),
                _ => {}
            }
            match tokio::fs::read_to_string(&full_path).await {
                Ok(content) => content,
                Err(err) => format!("Error reading {full_path}: {err}"),
            }
        }
        "session_workdir" => {
            let path = ptc.tool_call.arguments["path"]
                .as_str()
                .unwrap_or("")
                .to_string();
            execute_session_workdir(ctx, bus, session_id, &path, &ptc.cwd).await
        }
        "write_file" => {
            let path = ptc.tool_call.arguments["path"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let content = ptc.tool_call.arguments["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let full_path = match validate_tool_path(&path, &ptc.cwd) {
                Ok(p) => p,
                Err(e) => return e,
            };
            // Read old content for diff (empty if new file)
            let old_content = tokio::fs::read_to_string(&full_path)
                .await
                .unwrap_or_default();
            let previous = tokio::fs::read_to_string(&full_path)
                .await
                .unwrap_or_default();
            ctx.push_undo(session_id, &full_path, previous).await;
            if let Some(parent) = std::path::Path::new(&full_path).parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            match tokio::fs::write(&full_path, &content).await {
                Ok(()) => {
                    let mut msg = format!("Written {} bytes to {full_path}", content.len());
                    let diff = generate_unified_diff(&old_content, &content, &path, 3);
                    if !diff.is_empty() {
                        msg.push_str("\n\n");
                        msg.push_str(&diff);
                    }
                    msg
                }
                Err(err) => format!("Error writing {full_path}: {err}"),
            }
        }
        "edit_file" => execute_edit_file(ctx, session_id, ptc).await,
        "patch_file" => execute_patch_file(ctx, session_id, ptc).await,
        "list_files" => execute_list_files(ptc).await,
        "search_text" => execute_search_text(ptc).await,
        "undo" => execute_undo(ctx, session_id, ptc).await,
        "todo_write" => execute_todo_write(ctx, session_id, ptc).await,
        "todo_read" => execute_todo_read(ctx, session_id).await,
        "web_fetch" => execute_web_fetch(ctx, ptc).await,
        "web_search" => execute_web_search(ctx, ptc).await,
        "lsp_diagnostics" => execute_lsp_diagnostics(ctx, session_id, ptc).await,
        "lsp_hover" => execute_lsp_hover(ctx, session_id, ptc).await,
        "lsp_references" => execute_lsp_references(ctx, session_id, ptc).await,
        "lsp_symbols" => execute_lsp_symbols(ctx, session_id, ptc).await,
        "read_symbol" => execute_read_symbol(ctx, session_id, ptc).await,
        "patch_symbol" => execute_patch_symbol(ctx, session_id, ptc).await,
        "js_eval" => execute_js_eval(ptc).await,
        "js_script_save" => execute_js_script_save(ctx, session_id, ptc).await,
        "js_script_list" => execute_js_script_list(ctx, session_id).await,
        "js_script" => execute_js_script(ctx, session_id, ptc).await,
        "git_commit" => execute_git_commit(ptc).await,
        "plan_save" => execute_plan_save(ctx, bus, session_id, ptc).await,
        "plan_read" => execute_plan_read(ctx, session_id, ptc).await,
        "plan_update" => execute_plan_update(ctx, bus, session_id, ptc).await,
        other => format!("Unknown tool: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve a potentially relative path against the session cwd.
/// Handles `../`, `./`, and normalizes the result without requiring
/// the path to exist on disk (unlike std::fs::canonicalize).
fn resolve_path(path: &str, cwd: &str) -> String {
    use std::path::{Component, PathBuf};

    let raw = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        PathBuf::from(cwd).join(path)
    };

    // Normalize: resolve `.`, `..`, collapse separators
    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {} // skip
            other => normalized.push(other),
        }
    }
    normalized.to_string_lossy().to_string()
}

/// Resolve a path and validate it stays within the session cwd.
/// Returns Ok(full_path) or Err(user-friendly error message).
///
/// **Note on absolute paths:** Absolute paths are allowed by design because the
/// LLM legitimately needs to access files outside the working directory (e.g.
/// `/tmp`, system headers, config files). Every tool call is shown to the user
/// for approval before execution, so this does not bypass user consent.
pub fn validate_tool_path(path: &str, cwd: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("Error: path must not be empty".to_string());
    }
    let full = resolve_path(path, cwd);
    let cwd_normalized = resolve_path(cwd, "/");
    // Allow paths within cwd or absolute paths the user explicitly provided
    // (the LLM may legitimately reference /tmp, /etc for reading, etc.)
    // We only block relative paths that escape cwd via ../
    if !path.starts_with('/') && !full.starts_with(&cwd_normalized) {
        return Err(format!(
            "Error: path '{}' resolves to '{}' which is outside the working directory '{}'",
            path, full, cwd
        ));
    }
    // Block access to .bear/ directory — managed exclusively by the server
    if is_bear_dir_path(&full) {
        return Err(
            "Error: the .bear/ directory is managed by Bear and cannot be accessed directly."
                .to_string(),
        );
    }
    Ok(full)
}

/// Check if a resolved path falls inside a `.bear/` directory.
fn is_bear_dir_path(path: &str) -> bool {
    path.contains("/.bear/") || path.ends_with("/.bear")
}

// ---------------------------------------------------------------------------
// session_workdir
// ---------------------------------------------------------------------------

async fn execute_session_workdir(
    ctx: &dyn ToolContext,
    bus: &dyn ToolBus,
    session_id: Uuid,
    path: &str,
    cwd: &str,
) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "Error: path must not be empty".to_string();
    }

    // Resolve the target directory without invoking a shell (prevents command injection).
    let target = if std::path::Path::new(trimmed).is_absolute() {
        std::path::PathBuf::from(trimmed)
    } else {
        std::path::PathBuf::from(cwd).join(trimmed)
    };

    let canonical = match tokio::fs::canonicalize(&target).await {
        Ok(p) => p,
        Err(err) => return format!("Error: {err}"),
    };

    if !canonical.is_dir() {
        return format!("Error: '{}' is not a directory", canonical.display());
    }

    let new_cwd = canonical.to_string_lossy().to_string();

    ctx.set_session_cwd(session_id, new_cwd.clone()).await;

    // Load fresh workspace state from the new directory's .bear/
    let new_auto_approved = ctx.load_workspace_auto_approved(&new_cwd).await;
    ctx.reset_session_auto_approved(session_id, new_auto_approved)
        .await;

    bus.send(ServerMessage::Notice {
        text: format!("Working directory set to: {new_cwd}"),
    })
    .await;
    format!("Working directory changed to {new_cwd}")
}

// ---------------------------------------------------------------------------
// run_command
// ---------------------------------------------------------------------------

pub async fn execute_run_command(
    ctx: &dyn ToolContext,
    bus: &dyn ToolBus,
    session_id: Uuid,
    cmd_str: &str,
    cwd: &str,
) -> String {
    // Block shell commands that reference the .bear/ directory
    if cmd_str.contains(".bear") {
        return "Error: the .bear/ directory is managed by Bear and cannot be accessed via shell commands.".to_string();
    }

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd_str)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return format!("Failed to spawn: {err}"),
    };

    let pid = child.id().unwrap_or(0);
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(16);

    let proc_info = ProcessInfo {
        pid,
        command: cmd_str.to_string(),
        running: true,
    };

    ctx.register_process(session_id, pid, cmd_str.to_string(), stdin_tx)
        .await;

    bus.send(ServerMessage::ProcessStarted { info: proc_info })
        .await;

    let mut stdin_handle = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (output_tx, mut output_rx) = mpsc::channel::<String>(64);

    if let Some(stdout) = stdout {
        let tx = output_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }

    if let Some(stderr) = stderr {
        let tx = output_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }
    drop(output_tx);

    tokio::spawn(async move {
        while let Some(data) = stdin_rx.recv().await {
            if let Some(ref mut stdin) = stdin_handle {
                let _ = stdin.write_all(data.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            }
        }
    });

    let mut all_output = String::new();
    while let Some(line) = output_rx.recv().await {
        bus.send(ServerMessage::ProcessOutput {
            pid,
            text: line.clone(),
        })
        .await;
        all_output.push_str(&line);
        all_output.push('\n');
    }

    let status = child.wait().await;
    let code = status.ok().and_then(|s| s.code());

    ctx.mark_process_exited(pid).await;

    bus.send(ServerMessage::ProcessExited { pid, code }).await;

    if all_output.is_empty() {
        format!(
            "Process exited with code {}",
            code.map(|c| c.to_string()).unwrap_or("unknown".into())
        )
    } else {
        all_output
    }
}

// ---------------------------------------------------------------------------
// edit_file — surgical find/replace
// ---------------------------------------------------------------------------

async fn execute_edit_file(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let old_text = ptc.tool_call.arguments["old_text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let new_text = ptc.tool_call.arguments["new_text"]
        .as_str()
        .unwrap_or("")
        .to_string();

    if old_text.is_empty() {
        return "Error: old_text must not be empty".to_string();
    }

    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    let count = content.matches(&old_text).count();
    if count == 0 {
        return format!("Error: old_text not found in {full_path}");
    }
    if count > 1 {
        return format!(
            "Error: old_text found {count} times in {full_path}. Provide a more unique snippet."
        );
    }

    ctx.push_undo(session_id, &full_path, content.clone()).await;
    let updated = content.replacen(&old_text, &new_text, 1);
    match tokio::fs::write(&full_path, &updated).await {
        Ok(()) => {
            let mut msg = format!("Edited {full_path} (replaced 1 occurrence)");
            let diff = generate_unified_diff(&content, &updated, &path, 3);
            if !diff.is_empty() {
                msg.push_str("\n\n");
                msg.push_str(&diff);
            }
            msg
        }
        Err(err) => format!("Error writing {full_path}: {err}"),
    }
}

// ---------------------------------------------------------------------------
// patch_file — apply unified diff
// ---------------------------------------------------------------------------

async fn execute_patch_file(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let diff = ptc.tool_call.arguments["diff"]
        .as_str()
        .unwrap_or("")
        .to_string();

    tracing::debug!("patch_file: path={path:?}, diff length={}", diff.len());

    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    tracing::debug!("patch_file: file has {} lines", content.lines().count());

    match apply_unified_diff(&content, &diff) {
        Ok(patched) => {
            ctx.push_undo(session_id, &full_path, content.clone()).await;
            match tokio::fs::write(&full_path, &patched).await {
                Ok(()) => {
                    let mut msg = format!("Patched {full_path} successfully");
                    let udiff = generate_unified_diff(&content, &patched, &path, 3);
                    if !udiff.is_empty() {
                        msg.push_str("\n\n");
                        msg.push_str(&udiff);
                    }
                    msg
                }
                Err(err) => format!("Error writing {full_path}: {err}"),
            }
        }
        Err(err) => {
            tracing::warn!("patch_file failed on {full_path}: {err}");
            tracing::debug!("patch_file diff was:\n{diff}");
            format!("Patch failed: {err}")
        }
    }
}

// ---------------------------------------------------------------------------
// Unified diff applier with fuzzy hunk matching
// ---------------------------------------------------------------------------

/// Unified diff applier with fuzzy hunk matching.
///
/// Parses `@@ -old_start,old_count +new_start,new_count @@` hunks.
/// For each hunk, extracts the expected context/removal lines and searches
/// for the best match in the original file near the claimed position.
/// This tolerates LLM-generated diffs where line numbers are slightly off.
pub fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
    // Normalize \r\n to \n in diff (LLMs sometimes produce \r\n in JSON strings)
    let diff = diff.replace("\r\n", "\n");
    let diff = diff.as_str();
    let orig_lines: Vec<&str> = original.lines().collect();
    let diff_lines: Vec<&str> = diff.lines().collect();

    // --- Parse hunks -----------------------------------------------------------
    struct Hunk {
        claimed_old_start: usize, // 1-indexed from @@ header
        lines: Vec<HunkLine>,
    }
    #[derive(Clone)]
    enum HunkLine {
        Context(String),
        Remove(String),
        Add(String),
    }

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut di = 0;

    // Skip --- and +++ header lines if present
    while di < diff_lines.len() {
        let line = diff_lines[di];
        if line.starts_with("---") || line.starts_with("+++") {
            di += 1;
        } else {
            break;
        }
    }

    while di < diff_lines.len() {
        let line = diff_lines[di];
        if !line.starts_with("@@") {
            di += 1;
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            return Err(format!("Invalid hunk header: {line}"));
        }

        let old_range = parts[1].trim_start_matches('-');
        let claimed_old_start: usize = old_range
            .split(',')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        di += 1;
        let mut hunk_lines = Vec::new();
        while di < diff_lines.len() {
            let hline = diff_lines[di];
            if hline.starts_with("@@") {
                break;
            }
            if let Some(rest) = hline.strip_prefix('-') {
                hunk_lines.push(HunkLine::Remove(rest.to_string()));
            } else if let Some(rest) = hline.strip_prefix('+') {
                hunk_lines.push(HunkLine::Add(rest.to_string()));
            } else {
                // Context line (starts with ' ' or is bare text)
                let ctx = hline.strip_prefix(' ').unwrap_or(hline);
                hunk_lines.push(HunkLine::Context(ctx.to_string()));
            }
            di += 1;
        }

        hunks.push(Hunk {
            claimed_old_start,
            lines: hunk_lines,
        });
    }

    if hunks.is_empty() {
        return Err("No hunks found in diff".to_string());
    }

    // --- Apply hunks with fuzzy matching --------------------------------------

    let mut result_lines: Vec<String> = Vec::new();
    let mut orig_idx: usize = 0; // next unprocessed line in original (0-indexed)

    for (hunk_num, hunk) in hunks.iter().enumerate() {
        // Collect the "old" lines from this hunk (context + removal) in order.
        let old_lines_expected: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|hl| match hl {
                HunkLine::Context(s) => Some(s.as_str()),
                HunkLine::Remove(s) => Some(s.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();

        if old_lines_expected.is_empty() {
            // Pure insertion hunk — use claimed position
            let target = hunk.claimed_old_start.saturating_sub(1).max(orig_idx);
            while orig_idx < target && orig_idx < orig_lines.len() {
                result_lines.push(orig_lines[orig_idx].to_string());
                orig_idx += 1;
            }
            for hl in &hunk.lines {
                if let HunkLine::Add(s) = hl {
                    result_lines.push(s.clone());
                }
            }
            continue;
        }

        // Fuzzy search: find the best position for this hunk's old lines.
        let claimed_0 = hunk.claimed_old_start.saturating_sub(1);
        let search_start = orig_idx;
        let search_end = (claimed_0 + 200).min(orig_lines.len());
        let need = old_lines_expected.len();

        let find_match = |cmp: &dyn Fn(&str, &str) -> bool| -> Option<usize> {
            let scan_from = search_start;
            let scan_to = if search_end >= need {
                search_end - need + 1
            } else {
                scan_from
            };
            let mut best_pos: Option<usize> = None;
            let mut best_distance: usize = usize::MAX;

            for pos in scan_from..=scan_to.min(orig_lines.len().saturating_sub(need)) {
                let matches = old_lines_expected.iter().enumerate().all(|(k, &expected)| {
                    pos + k < orig_lines.len() && cmp(orig_lines[pos + k], expected)
                });
                if matches {
                    let distance = pos.abs_diff(claimed_0);
                    if distance < best_distance {
                        best_distance = distance;
                        best_pos = Some(pos);
                    }
                    if distance == 0 {
                        break;
                    }
                }
            }
            best_pos
        };

        // Pass 1: exact match
        // Pass 2: trailing-whitespace-trimmed match
        let match_pos = find_match(&|a: &str, b: &str| a == b)
            .or_else(|| find_match(&|a: &str, b: &str| a.trim_end() == b.trim_end()));

        let match_pos = match match_pos {
            Some(p) => p,
            None => {
                let mut mismatch_info = String::new();
                if claimed_0 < orig_lines.len() && !old_lines_expected.is_empty() {
                    for (k, &expected) in old_lines_expected.iter().enumerate().take(10) {
                        if claimed_0 + k < orig_lines.len() {
                            let actual = orig_lines[claimed_0 + k];
                            if actual != expected {
                                mismatch_info = format!(
                                    "\nFirst mismatch at offset {k} (file line {}):\n  \
                                     expected ({} bytes): {:?}\n  \
                                     actual   ({} bytes): {:?}",
                                    claimed_0 + k + 1,
                                    expected.len(),
                                    expected,
                                    actual.len(),
                                    actual,
                                );
                                break;
                            }
                        } else {
                            mismatch_info = format!(
                                "\nFile has only {} lines but hunk expects {} old lines",
                                orig_lines.len(),
                                need,
                            );
                            break;
                        }
                    }
                    if mismatch_info.is_empty() && need > 10 {
                        mismatch_info = format!(
                            "\nFirst 10 lines match; mismatch is deeper in the hunk ({need} old lines total). \
                             scan_start={search_start}, scan_end={search_end}, need={need}",
                        );
                    }
                }

                let ctx_start = claimed_0.min(orig_lines.len());
                let ctx_end = (claimed_0 + need + 2).min(orig_lines.len());
                let actual_ctx: Vec<String> = (ctx_start..ctx_end)
                    .take(10)
                    .map(|i| format!("  {}: {}", i + 1, orig_lines[i]))
                    .collect();
                let expected_ctx: Vec<String> = old_lines_expected
                    .iter()
                    .enumerate()
                    .take(5)
                    .map(|(i, l)| format!("  {i}: {l}"))
                    .collect();
                return Err(format!(
                    "Hunk {} failed: could not find matching lines near line {}.\n\
                     Expected:\n{}\n\
                     Actual file around that area:\n{}{mismatch_info}",
                    hunk_num + 1,
                    hunk.claimed_old_start,
                    expected_ctx.join("\n"),
                    actual_ctx.join("\n"),
                ));
            }
        };

        // Copy original lines before this hunk
        while orig_idx < match_pos {
            result_lines.push(orig_lines[orig_idx].to_string());
            orig_idx += 1;
        }

        // Apply hunk lines — always use original file content for context
        for hl in &hunk.lines {
            match hl {
                HunkLine::Context(_) => {
                    if orig_idx < orig_lines.len() {
                        result_lines.push(orig_lines[orig_idx].to_string());
                    }
                    orig_idx += 1;
                }
                HunkLine::Remove(_) => {
                    orig_idx += 1;
                }
                HunkLine::Add(s) => {
                    result_lines.push(s.clone());
                }
            }
        }
    }

    // Copy remaining original lines
    while orig_idx < orig_lines.len() {
        result_lines.push(orig_lines[orig_idx].to_string());
        orig_idx += 1;
    }

    // Preserve trailing newline if original had one
    let mut output = result_lines.join("\n");
    if original.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Unified diff generation
// ---------------------------------------------------------------------------

/// Generate a unified diff between two strings, with `context` lines of context.
/// Returns an empty string if the contents are identical.
pub fn generate_unified_diff(old: &str, new: &str, path: &str, context: usize) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let n = old_lines.len();
    let m = new_lines.len();

    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut edits: Vec<(char, usize, usize)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && old_lines[i] == new_lines[j] {
            edits.push(('E', i, j));
            i += 1;
            j += 1;
        } else if i < n && (j >= m || dp[i + 1][j] >= dp[i][j + 1]) {
            edits.push(('D', i, j));
            i += 1;
        } else {
            edits.push(('I', i, j));
            j += 1;
        }
    }

    if edits.iter().all(|(op, _, _)| *op == 'E') {
        return String::new();
    }

    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut hunk_start: Option<usize> = None;
    let mut last_change: Option<usize> = None;

    for (idx, (op, _, _)) in edits.iter().enumerate() {
        if *op != 'E' {
            if hunk_start.is_none() {
                hunk_start = Some(idx.saturating_sub(context));
            }
            last_change = Some(idx);
        }
    }

    if let (Some(start), Some(last)) = (hunk_start, last_change) {
        let end = (last + context + 1).min(edits.len());
        hunks.push((start, end));
    }

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));

    for (start, end) in &hunks {
        let mut old_start = 0usize;
        let mut old_count = 0usize;
        let mut new_start = 0usize;
        let mut new_count = 0usize;
        let mut first = true;

        for &(op, oi, ni) in &edits[*start..*end] {
            if first {
                old_start = oi + 1;
                new_start = ni + 1;
                first = false;
            }
            match op {
                'E' => {
                    old_count += 1;
                    new_count += 1;
                }
                'D' => {
                    old_count += 1;
                }
                'I' => {
                    new_count += 1;
                }
                _ => {}
            }
        }

        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start, old_count, new_start, new_count
        ));

        for &(op, oi, ni) in &edits[*start..*end] {
            match op {
                'E' => out.push_str(&format!(" {}\n", old_lines[oi])),
                'D' => out.push_str(&format!("-{}\n", old_lines[oi])),
                'I' => out.push_str(&format!("+{}\n", new_lines[ni])),
                _ => {}
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// list_files
// ---------------------------------------------------------------------------

async fn execute_list_files(ptc: &PendingToolCall) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or(".")
        .to_string();
    let pattern = ptc.tool_call.arguments["pattern"]
        .as_str()
        .map(|s| s.to_string());
    let max_depth = ptc.tool_call.arguments["max_depth"].as_u64().unwrap_or(3) as usize;

    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let result = tokio::task::spawn_blocking(move || {
        list_files_sync(&full_path, pattern.as_deref(), max_depth)
    })
    .await;

    match result {
        Ok(output) => output,
        Err(err) => format!("Error: {err}"),
    }
}

fn list_files_sync(root: &str, pattern: Option<&str>, max_depth: usize) -> String {
    use std::fs;
    use std::path::Path;

    let root_path = Path::new(root);
    if !root_path.exists() {
        return format!("Error: path does not exist: {root}");
    }
    if !root_path.is_dir() {
        let meta = fs::metadata(root_path).ok();
        let size = meta.map(|m| m.len()).unwrap_or(0);
        return format!("f {size:>8}  {root}");
    }

    let glob_pattern = pattern.and_then(|p| glob::Pattern::new(p).ok());
    let mut entries = Vec::new();
    collect_entries(
        root_path,
        root_path,
        0,
        max_depth,
        &glob_pattern,
        &mut entries,
    );

    if entries.is_empty() {
        "No matching files found.".to_string()
    } else {
        entries.join("\n")
    }
}

fn collect_entries(
    base: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    pattern: &Option<glob::Pattern>,
    out: &mut Vec<String>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());

    for entry in items {
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path);
        let name = rel.to_string_lossy();

        if entry
            .file_name()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }

        if path.is_dir() {
            out.push(format!("d          {name}/"));
            collect_entries(base, &path, depth + 1, max_depth, pattern, out);
        } else {
            if let Some(ref pat) = pattern {
                if !pat.matches(&name) {
                    continue;
                }
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(format!("f {size:>8}  {name}"));
        }
    }
}

// ---------------------------------------------------------------------------
// search_text
// ---------------------------------------------------------------------------

async fn execute_search_text(ptc: &PendingToolCall) -> String {
    let pattern = ptc.tool_call.arguments["pattern"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or(".")
        .to_string();
    let include = ptc.tool_call.arguments["include"]
        .as_str()
        .map(|s| s.to_string());
    let max_results = ptc.tool_call.arguments["max_results"]
        .as_u64()
        .unwrap_or(50) as usize;

    if pattern.is_empty() {
        return "Error: pattern must not be empty".to_string();
    }

    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let result = tokio::task::spawn_blocking(move || {
        search_text_sync(&full_path, &pattern, include.as_deref(), max_results)
    })
    .await;

    match result {
        Ok(output) => output,
        Err(err) => format!("Error: {err}"),
    }
}

fn search_text_sync(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    max_results: usize,
) -> String {
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(err) => return format!("Invalid regex: {err}"),
    };

    let include_glob = include.and_then(|p| glob::Pattern::new(p).ok());
    let root_path = std::path::Path::new(root);
    let mut results = Vec::new();

    if root_path.is_file() {
        search_file(root_path, &re, &mut results, max_results);
    } else {
        search_dir(
            root_path,
            &re,
            &include_glob,
            &mut results,
            max_results,
            0,
            10,
        );
    }

    if results.is_empty() {
        "No matches found.".to_string()
    } else {
        let truncated = if results.len() >= max_results {
            format!("\n[… results capped at {max_results}]")
        } else {
            String::new()
        };
        format!("{}{truncated}", results.join("\n"))
    }
}

fn search_dir(
    dir: &std::path::Path,
    re: &regex::Regex,
    include: &Option<glob::Pattern>,
    results: &mut Vec<String>,
    max_results: usize,
    depth: usize,
    max_depth: usize,
) {
    if depth > max_depth || results.len() >= max_results {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        if results.len() >= max_results {
            return;
        }
        let path = entry.path();
        if entry
            .file_name()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        if path.is_dir() {
            search_dir(
                &path,
                re,
                include,
                results,
                max_results,
                depth + 1,
                max_depth,
            );
        } else {
            if let Some(ref pat) = include {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !pat.matches(name) {
                    continue;
                }
            }
            search_file(&path, re, results, max_results);
        }
    }
}

fn search_file(
    path: &std::path::Path,
    re: &regex::Regex,
    results: &mut Vec<String>,
    max_results: usize,
) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let reader = std::io::BufReader::new(file);
    let display_path = path.to_string_lossy();
    for (line_num, line) in reader.lines().enumerate() {
        if results.len() >= max_results {
            return;
        }
        let Ok(line) = line else { return };
        if re.is_match(&line) {
            results.push(format!("{}:{}: {}", display_path, line_num + 1, line));
        }
    }
}

// ---------------------------------------------------------------------------
// undo
// ---------------------------------------------------------------------------

async fn execute_undo(ctx: &dyn ToolContext, session_id: Uuid, ptc: &PendingToolCall) -> String {
    let steps = ptc.tool_call.arguments["steps"]
        .as_u64()
        .unwrap_or(1)
        .min(10) as usize;

    let entries = ctx.get_undo_entries(session_id, steps).await;
    if entries.is_empty() {
        return "Nothing to undo.".to_string();
    }

    let mut output = Vec::new();
    for entry in &entries {
        match tokio::fs::write(&entry.path, &entry.previous_content).await {
            Ok(()) => output.push(format!("Restored {}", entry.path)),
            Err(err) => output.push(format!("Error restoring {}: {err}", entry.path)),
        }
    }
    output.join("\n")
}

// ---------------------------------------------------------------------------
// todo_write / todo_read
// ---------------------------------------------------------------------------

async fn execute_todo_write(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let items = match ptc.tool_call.arguments["items"].as_array() {
        Some(arr) => arr,
        None => return "Error: todo_write requires an 'items' array.".to_string(),
    };

    let mut todo_list = Vec::new();
    for item in items {
        let id = item["id"].as_str().unwrap_or("").to_string();
        let content = item["content"].as_str().unwrap_or("").to_string();
        let status = item["status"].as_str().unwrap_or("pending").to_string();
        let priority = item["priority"].as_str().unwrap_or("medium").to_string();
        if content.is_empty() {
            continue;
        }
        todo_list.push(TodoItem {
            id,
            content,
            status,
            priority,
        });
    }

    let count = todo_list.len();
    ctx.set_todo_list(session_id, todo_list).await;
    format!("Todo list updated ({count} items).")
}

async fn execute_todo_read(ctx: &dyn ToolContext, session_id: Uuid) -> String {
    let todo_list = ctx.get_todo_list(session_id).await;
    if todo_list.is_empty() {
        return "Todo list is empty.".to_string();
    }
    let mut lines = Vec::new();
    for item in &todo_list {
        let status_icon = match item.status.as_str() {
            "completed" => "✓",
            "in_progress" => "→",
            _ => "○",
        };
        let priority_tag = match item.priority.as_str() {
            "high" => " [HIGH]",
            "low" => " [LOW]",
            _ => "",
        };
        lines.push(format!(
            "{status_icon} [{}] {}{priority_tag}",
            item.id, item.content
        ));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// plan_save / plan_read / plan_update
// ---------------------------------------------------------------------------

async fn execute_plan_save(
    ctx: &dyn ToolContext,
    bus: &dyn ToolBus,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let name = match ptc.tool_call.arguments["name"].as_str() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "Error: plan_save requires a 'name' argument.".to_string(),
    };

    // Validate name: [a-z0-9_-]+ only
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
        return "Error: plan name must match [a-z0-9_-]+.".to_string();
    }

    let title = ptc.tool_call.arguments["title"]
        .as_str()
        .unwrap_or(&name)
        .to_string();

    let steps_arr = match ptc.tool_call.arguments["steps"].as_array() {
        Some(arr) => arr,
        None => return "Error: plan_save requires a 'steps' array.".to_string(),
    };

    let mut steps = Vec::new();
    for item in steps_arr {
        let id = item["id"].as_str().unwrap_or("").to_string();
        let description = item["description"].as_str().unwrap_or("").to_string();
        let status = item["status"].as_str().unwrap_or("pending").to_string();
        let detail = item["detail"].as_str().map(|s| s.to_string());
        if !description.is_empty() {
            steps.push(crate::workspace::PlanStep {
                id,
                description,
                status,
                detail,
            });
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found.".to_string(),
    };

    // Try to load existing plan to preserve created_at
    let created_at = if let Ok(existing) = ctx.load_plan(&cwd, &name).await {
        existing.created_at
    } else {
        now.clone()
    };

    let mut plan = crate::workspace::SavedPlan {
        name: name.clone(),
        title: title.clone(),
        steps: steps.clone(),
        status: String::new(),
        created_at,
        updated_at: now,
    };
    plan.recompute_status();

    match ctx.save_plan(&cwd, &plan).await {
        Ok(()) => {
            bus.send(ServerMessage::PlanUpdate {
                name: plan.name,
                title: plan.title,
                status: plan.status,
                steps: plan.steps,
            })
            .await;
            format!("Plan '{name}' saved ({} steps).", steps.len())
        }
        Err(e) => format!("Error saving plan: {e}"),
    }
}

async fn execute_plan_read(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found.".to_string(),
    };

    let name = ptc.tool_call.arguments["name"].as_str().unwrap_or("");

    if name.is_empty() {
        // List all plans
        let plans = ctx.list_plans(&cwd).await;
        if plans.is_empty() {
            return "No plans found.".to_string();
        }
        let mut lines = Vec::new();
        for p in &plans {
            let done = p.steps.iter().filter(|s| s.status == "completed").count();
            let total = p.steps.len();
            lines.push(format!(
                "[{}] {} — {} ({}/{} done)",
                p.status, p.name, p.title, done, total
            ));
        }
        lines.join("\n")
    } else {
        // Read specific plan
        match ctx.load_plan(&cwd, name).await {
            Ok(plan) => {
                let mut lines = vec![format!(
                    "Plan: {} — {} [{}]",
                    plan.name, plan.title, plan.status
                )];
                for step in &plan.steps {
                    let icon = match step.status.as_str() {
                        "completed" => "✓",
                        "in_progress" => "→",
                        "failed" => "✗",
                        _ => "○",
                    };
                    let detail_str = step
                        .detail
                        .as_ref()
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default();
                    lines.push(format!("  {icon} {}: {}{detail_str}", step.id, step.description));
                }
                lines.join("\n")
            }
            Err(e) => format!("Error: {e}"),
        }
    }
}

async fn execute_plan_update(
    ctx: &dyn ToolContext,
    bus: &dyn ToolBus,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let name = match ptc.tool_call.arguments["name"].as_str() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "Error: plan_update requires a 'name' argument.".to_string(),
    };
    let step_id = match ptc.tool_call.arguments["step_id"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return "Error: plan_update requires a 'step_id' argument.".to_string(),
    };
    let new_status = match ptc.tool_call.arguments["status"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return "Error: plan_update requires a 'status' argument.".to_string(),
    };
    let detail = ptc.tool_call.arguments["detail"]
        .as_str()
        .map(|s| s.to_string());

    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found.".to_string(),
    };

    let mut plan = match ctx.load_plan(&cwd, &name).await {
        Ok(p) => p,
        Err(e) => return format!("Error: {e}"),
    };

    let step = match plan.steps.iter_mut().find(|s| s.id == step_id) {
        Some(s) => s,
        None => return format!("Error: step '{step_id}' not found in plan '{name}'."),
    };
    step.status = new_status;
    step.detail = detail;

    plan.updated_at = chrono::Utc::now().to_rfc3339();
    plan.recompute_status();

    match ctx.save_plan(&cwd, &plan).await {
        Ok(()) => {
            bus.send(ServerMessage::PlanUpdate {
                name: plan.name,
                title: plan.title,
                status: plan.status,
                steps: plan.steps,
            })
            .await;
            format!("Plan '{name}' step '{step_id}' updated.")
        }
        Err(e) => format!("Error saving plan: {e}"),
    }
}

// ---------------------------------------------------------------------------
// web_fetch / web_search
// ---------------------------------------------------------------------------

async fn execute_web_fetch(ctx: &dyn ToolContext, ptc: &PendingToolCall) -> String {
    let url = match ptc.tool_call.arguments["url"].as_str() {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => return "Error: web_fetch requires a 'url' argument.".to_string(),
    };
    let max_chars = ptc.tool_call.arguments["max_chars"]
        .as_u64()
        .unwrap_or(10_000) as usize;

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return "Error: URL must start with http:// or https://".to_string();
    }

    let response = match ctx
        .http_client()
        .get(&url)
        .header("User-Agent", "Bear/1.0 (AI coding assistant)")
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => return format!("Error fetching {url}: {err}"),
    };

    let status = response.status();
    if !status.is_success() {
        return format!("Error: HTTP {status} for {url}");
    }

    let body = match response.text().await {
        Ok(t) => t,
        Err(err) => return format!("Error reading response body: {err}"),
    };

    let text = html_to_markdown(&body);

    if text.len() > max_chars {
        let mut end = max_chars;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n\n[... truncated at {end} bytes, total {} bytes]",
            &text[..end],
            text.len()
        )
    } else {
        text
    }
}

/// Convert HTML to Markdown using html-to-markdown-rs. Falls back to strip_html_tags on error.
pub fn html_to_markdown(html: &str) -> String {
    match html_to_markdown_rs::convert(html, None) {
        Ok(md) => collapse_whitespace(&md),
        Err(_) => collapse_whitespace(&strip_html_tags(html)),
    }
}

/// Case-insensitive prefix check on a byte slice without allocating a
/// lowercased copy of the entire input.
fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
}

/// Simple HTML tag stripper — removes tags, decodes common entities, collapses whitespace.
/// All indexing uses **byte** offsets so that multi-byte UTF-8 characters
/// (e.g. `·`) never cause a panic. Text content preserves original casing.
pub fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;

    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if !in_tag && starts_with_ignore_ascii_case(&bytes[i..], b"<script") {
            in_script = true;
            in_tag = true;
            i += 1;
            continue;
        }
        if in_script && starts_with_ignore_ascii_case(&bytes[i..], b"</script>") {
            in_script = false;
            in_tag = false;
            i += 9;
            continue;
        }
        if !in_tag && starts_with_ignore_ascii_case(&bytes[i..], b"<style") {
            in_style = true;
            in_tag = true;
            i += 1;
            continue;
        }
        if in_style && starts_with_ignore_ascii_case(&bytes[i..], b"</style>") {
            in_style = false;
            in_tag = false;
            i += 8;
            continue;
        }
        if in_script || in_style {
            i += 1;
            continue;
        }

        if bytes[i] == b'<' {
            in_tag = true;
            let rest = &bytes[i..];
            if starts_with_ignore_ascii_case(rest, b"<br")
                || starts_with_ignore_ascii_case(rest, b"<p ")
                || starts_with_ignore_ascii_case(rest, b"<p>")
                || starts_with_ignore_ascii_case(rest, b"<div")
                || starts_with_ignore_ascii_case(rest, b"<li")
                || starts_with_ignore_ascii_case(rest, b"<h1")
                || starts_with_ignore_ascii_case(rest, b"<h2")
                || starts_with_ignore_ascii_case(rest, b"<h3")
                || starts_with_ignore_ascii_case(rest, b"<tr")
            {
                result.push('\n');
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'>' {
            in_tag = false;
            i += 1;
            continue;
        }
        if !in_tag {
            if bytes[i] == b'&' {
                if html[i..].starts_with("&lt;") {
                    result.push('<');
                    i += 4;
                    continue;
                }
                if html[i..].starts_with("&gt;") {
                    result.push('>');
                    i += 4;
                    continue;
                }
                if html[i..].starts_with("&amp;") {
                    result.push('&');
                    i += 5;
                    continue;
                }
                if html[i..].starts_with("&quot;") {
                    result.push('"');
                    i += 6;
                    continue;
                }
                if html[i..].starts_with("&nbsp;") {
                    result.push(' ');
                    i += 6;
                    continue;
                }
                if html[i..].starts_with("&#39;") {
                    result.push('\'');
                    i += 5;
                    continue;
                }
            }
            if let Some(ch) = html[i..].chars().next() {
                result.push(ch);
                i += ch.len_utf8();
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    result
}

/// Collapse runs of whitespace into single spaces/newlines.
pub fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_was_newline = false;
    let mut prev_was_space = false;

    for ch in text.chars() {
        if ch == '\n' {
            if !prev_was_newline {
                result.push('\n');
            }
            prev_was_newline = true;
            prev_was_space = false;
        } else if ch.is_whitespace() {
            if !prev_was_space && !prev_was_newline {
                result.push(' ');
            }
            prev_was_space = true;
        } else {
            prev_was_newline = false;
            prev_was_space = false;
            result.push(ch);
        }
    }
    result.trim().to_string()
}

async fn execute_web_search(ctx: &dyn ToolContext, ptc: &PendingToolCall) -> String {
    let query = match ptc.tool_call.arguments["query"].as_str() {
        Some(q) if !q.is_empty() => q.to_string(),
        _ => return "Error: web_search requires a 'query' argument.".to_string(),
    };
    let max_results = ptc.tool_call.arguments["max_results"].as_u64().unwrap_or(5) as usize;

    // Fallback chain: DDG → Google → Brave → error
    let mut last_error;

    // 1. Try DuckDuckGo (no API key needed)
    match search_ddg(ctx.http_client(), &query, max_results).await {
        Ok(results) => return results,
        Err(err) => {
            last_error = format!("DuckDuckGo: {err}");
        }
    }

    // 2. Try Google Custom Search (if keys present)
    if let (Some(api_key), Some(cx)) = (ctx.google_api_key(), ctx.google_cx()) {
        match search_google(ctx.http_client(), api_key, cx, &query, max_results).await {
            Ok(results) => return results,
            Err(err) => {
                last_error = format!("Google: {err}");
            }
        }
    }

    // 3. Try Brave Search (if key present)
    if let Some(api_key) = ctx.brave_api_key() {
        match search_brave(ctx.http_client(), api_key, &query, max_results).await {
            Ok(results) => return results,
            Err(err) => {
                last_error = format!("Brave: {err}");
            }
        }
    }

    format!("Error: web search is temporarily unavailable. Last error: {last_error}")
}

// ---------------------------------------------------------------------------
// DuckDuckGo HTML search
// ---------------------------------------------------------------------------

const DDG_USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
];

async fn search_ddg(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<String, String> {
    let encoded_query = urlencoding::encode(query);
    let url = format!("https://html.duckduckgo.com/html/?q={encoded_query}");

    // Try with different User-Agents to reduce CAPTCHA rate
    for ua in DDG_USER_AGENTS {
        let response = client
            .get(&url)
            .header("User-Agent", *ua)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status();

        // DDG returns 202 for CAPTCHA/bot detection — try next UA
        if status.as_u16() == 202 {
            continue;
        }

        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }

        let body = response
            .text()
            .await
            .map_err(|e| format!("body read failed: {e}"))?;

        // Detect CAPTCHA even on 200 (DDG sometimes returns 200 with CAPTCHA)
        if body.contains("anomaly-modal")
            || body.contains("Please complete the following challenge")
        {
            continue;
        }

        let parsed = parse_ddg_results(&body, max_results);
        if parsed == "No search results found." {
            return Err("no results (possibly rate-limited)".to_string());
        }
        return Ok(parsed);
    }

    Err("rate-limited by DuckDuckGo (CAPTCHA on all attempts)".to_string())
}

/// Parse DuckDuckGo HTML search results page.
fn parse_ddg_results(html: &str, max_results: usize) -> String {
    let mut results = Vec::new();

    let mut pos = 0;
    while results.len() < max_results {
        let marker = "class=\"result__a\"";
        let Some(marker_pos) = html[pos..].find(marker) else {
            break;
        };
        let abs_pos = pos + marker_pos;

        let search_back_start = abs_pos.saturating_sub(200);
        let href_end = (abs_pos + marker.len() + 200).min(html.len());
        let href = extract_href(&html[search_back_start..href_end]);

        let after_marker = abs_pos + marker.len();
        let title_start = html[after_marker..].find('>').map(|p| after_marker + p + 1);
        let title_end = title_start.and_then(|s| html[s..].find("</a>").map(|p| s + p));
        let title = match (title_start, title_end) {
            (Some(s), Some(e)) => strip_html_tags(&html[s..e]).trim().to_string(),
            _ => String::new(),
        };

        pos = title_end.unwrap_or(after_marker + 1);

        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(sp) = html[pos..].find(snippet_marker) {
            let sp_abs = pos + sp + snippet_marker.len();
            let sn_start = html[sp_abs..].find('>').map(|p| sp_abs + p + 1);
            let sn_end = sn_start.and_then(|s| {
                html[s..]
                    .find("</a>")
                    .or_else(|| html[s..].find("</span>"))
                    .map(|p| s + p)
            });
            match (sn_start, sn_end) {
                (Some(s), Some(e)) => strip_html_tags(&html[s..e]).trim().to_string(),
                _ => String::new(),
            }
        } else {
            String::new()
        };

        if !title.is_empty() || !href.is_empty() {
            results.push(format!(
                "{}. {}\n   {}\n   {}",
                results.len() + 1,
                if title.is_empty() {
                    "(no title)"
                } else {
                    &title
                },
                if href.is_empty() { "(no url)" } else { &href },
                if snippet.is_empty() {
                    "(no snippet)"
                } else {
                    &snippet
                },
            ));
        }
    }

    if results.is_empty() {
        "No search results found.".to_string()
    } else {
        results.join("\n\n")
    }
}

/// Extract href value from an HTML tag fragment.
fn extract_href(fragment: &str) -> String {
    if let Some(href_pos) = fragment.find("href=\"") {
        let start = href_pos + 6;
        if let Some(end) = fragment[start..].find('"') {
            let raw = &fragment[start..start + end];
            if let Some(uddg_pos) = raw.find("uddg=") {
                let url_start = uddg_pos + 5;
                let url_end = raw[url_start..].find('&').unwrap_or(raw.len() - url_start);
                let encoded = &raw[url_start..url_start + url_end];
                return urlencoding::decode(encoded)
                    .unwrap_or_else(|_| encoded.into())
                    .to_string();
            }
            return raw.to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Google Custom Search API
// ---------------------------------------------------------------------------

async fn search_google(
    client: &reqwest::Client,
    api_key: &str,
    cx: &str,
    query: &str,
    max_results: usize,
) -> Result<String, String> {
    let num = max_results.min(10); // Google CSE max is 10 per request
    let url = format!(
        "https://www.googleapis.com/customsearch/v1?key={}&cx={}&q={}&num={}",
        urlencoding::encode(api_key),
        urlencoding::encode(cx),
        urlencoding::encode(query),
        num,
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = response.status();
    if status.as_u16() == 429 {
        return Err("rate-limited (HTTP 429)".to_string());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("JSON parse failed: {e}"))?;

    let items = body["items"].as_array();
    let Some(items) = items else {
        return Err("no results in response".to_string());
    };

    let mut results = Vec::new();
    for (i, item) in items.iter().enumerate().take(max_results) {
        let title = item["title"].as_str().unwrap_or("(no title)");
        let link = item["link"].as_str().unwrap_or("(no url)");
        let snippet = item["snippet"].as_str().unwrap_or("(no snippet)");
        results.push(format!("{}. {}\n   {}\n   {}", i + 1, title, link, snippet));
    }

    if results.is_empty() {
        Err("no results".to_string())
    } else {
        Ok(results.join("\n\n"))
    }
}

// ---------------------------------------------------------------------------
// Brave Search API
// ---------------------------------------------------------------------------

async fn search_brave(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<String, String> {
    let count = max_results.min(20); // Brave max is 20
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding::encode(query),
        count,
    );

    let response = client
        .get(&url)
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = response.status();
    if status.as_u16() == 429 {
        return Err("rate-limited (HTTP 429)".to_string());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("JSON parse failed: {e}"))?;

    let results_arr = body["web"]["results"].as_array();
    let Some(items) = results_arr else {
        return Err("no results in response".to_string());
    };

    let mut results = Vec::new();
    for (i, item) in items.iter().enumerate().take(max_results) {
        let title = item["title"].as_str().unwrap_or("(no title)");
        let link = item["url"].as_str().unwrap_or("(no url)");
        let snippet = item["description"].as_str().unwrap_or("(no snippet)");
        results.push(format!("{}. {}\n   {}\n   {}", i + 1, title, link, snippet));
    }

    if results.is_empty() {
        Err("no results".to_string())
    } else {
        Ok(results.join("\n\n"))
    }
}

// ---------------------------------------------------------------------------
// LSP tools
// ---------------------------------------------------------------------------

async fn execute_lsp_diagnostics(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if path.is_empty() {
        return "Error: lsp_diagnostics requires a 'path' argument.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };
    match ctx.lsp_diagnostics(&full_path, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

async fn execute_lsp_hover(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let line = ptc.tool_call.arguments["line"].as_u64().unwrap_or(0) as u32;
    let character = ptc.tool_call.arguments["character"].as_u64().unwrap_or(0) as u32;
    if path.is_empty() {
        return "Error: lsp_hover requires a 'path' argument.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };
    let lsp_line = if line > 0 { line - 1 } else { 0 };
    let lsp_char = if character > 0 { character - 1 } else { 0 };
    match ctx.lsp_hover(&full_path, lsp_line, lsp_char, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

async fn execute_lsp_references(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let line = ptc.tool_call.arguments["line"].as_u64().unwrap_or(0) as u32;
    let character = ptc.tool_call.arguments["character"].as_u64().unwrap_or(0) as u32;
    if path.is_empty() {
        return "Error: lsp_references requires a 'path' argument.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };
    let lsp_line = if line > 0 { line - 1 } else { 0 };
    let lsp_char = if character > 0 { character - 1 } else { 0 };
    match ctx
        .lsp_references(&full_path, lsp_line, lsp_char, &cwd)
        .await
    {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

async fn execute_lsp_symbols(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if path.is_empty() {
        return "Error: lsp_symbols requires a 'path' argument.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };
    match ctx.lsp_symbols(&full_path, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// read_symbol
// ---------------------------------------------------------------------------

async fn execute_read_symbol(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let symbol = ptc.tool_call.arguments["symbol"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if path.is_empty() || symbol.is_empty() {
        return "Error: read_symbol requires 'path' and 'symbol' arguments.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };

    let (start_line, end_line) = match ctx.lsp_find_symbol_range(&full_path, &symbol, &cwd).await {
        Ok(range) => range,
        Err(e) => return e,
    };

    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let start = start_line as usize;
    let end = (end_line as usize + 1).min(lines.len());

    if start >= lines.len() {
        return format!(
            "Error: symbol range {start_line}-{end_line} is out of bounds (file has {} lines)",
            lines.len()
        );
    }

    let context_before = 2;
    let effective_start = start.saturating_sub(context_before);

    let mut result = format!("// {full_path} lines {}-{}\n", effective_start + 1, end);
    for (i, line) in lines[effective_start..end].iter().enumerate() {
        let line_num = effective_start + i + 1;
        result.push_str(&format!("{line_num:>5} | {line}\n"));
    }
    result
}

// ---------------------------------------------------------------------------
// patch_symbol
// ---------------------------------------------------------------------------

async fn execute_patch_symbol(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let symbol = ptc.tool_call.arguments["symbol"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let new_content = ptc.tool_call.arguments["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if path.is_empty() || symbol.is_empty() {
        return "Error: patch_symbol requires 'path', 'symbol', and 'content' arguments."
            .to_string();
    }
    if new_content.is_empty() {
        return "Error: patch_symbol 'content' must not be empty.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };

    let (start_line, end_line) = match ctx.lsp_find_symbol_range(&full_path, &symbol, &cwd).await {
        Ok(range) => range,
        Err(e) => return e,
    };

    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let start = start_line as usize;
    let end = (end_line as usize + 1).min(lines.len());

    if start >= lines.len() {
        return format!(
            "Error: symbol range {start_line}-{end_line} is out of bounds (file has {} lines)",
            lines.len()
        );
    }

    ctx.push_undo(session_id, &full_path, content.clone()).await;

    let mut new_file = String::new();
    for line in &lines[..start] {
        new_file.push_str(line);
        new_file.push('\n');
    }
    new_file.push_str(&new_content);
    if !new_content.ends_with('\n') {
        new_file.push('\n');
    }
    for line in &lines[end..] {
        new_file.push_str(line);
        new_file.push('\n');
    }
    if !content.ends_with('\n') && new_file.ends_with('\n') {
        new_file.pop();
    }

    match tokio::fs::write(&full_path, &new_file).await {
        Ok(()) => {
            let new_lines = new_content.lines().count();
            let old_lines = end - start;
            let diff = generate_unified_diff(&content, &new_file, &path, 3);
            let mut msg = format!(
                "Patched symbol '{}' in {} (replaced {} lines with {} lines)",
                symbol, full_path, old_lines, new_lines
            );
            if !diff.is_empty() {
                msg.push_str("\n\n");
                msg.push_str(&diff);
            }
            msg
        }
        Err(err) => format!("Error writing {full_path}: {err}"),
    }
}

// ---------------------------------------------------------------------------
// Utility functions used by the agent loop (exported)
// ---------------------------------------------------------------------------

/// Extract individual command names from a shell string.
pub fn extract_shell_commands(cmd_str: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    #[allow(clippy::collapsible_str_replace)]
    let replaced = cmd_str
        .replace("&&", "\x00")
        .replace("||", "\x00")
        .replace(';', "\x00")
        .replace('|', "\x00");
    let segments: Vec<&str> = replaced
        .split('\x00')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_start_matches('(').trim_start_matches("$(").trim())
        .collect();

    for seg in segments {
        let tokens: Vec<&str> = seg.split_whitespace().collect();
        for token in &tokens {
            if token.contains('=') && !token.starts_with('-') {
                continue;
            }
            if *token == "sudo"
                || *token == "env"
                || *token == "nohup"
                || *token == "time"
                || *token == "nice"
            {
                continue;
            }
            let base = token.rsplit('/').next().unwrap_or(token);
            if !base.is_empty() && seen.insert(base.to_string()) {
                result.push(base.to_string());
            }
            break;
        }
    }

    result
}

/// Map internal tool names to their user-facing display names.
pub fn tool_display_name(name: &str) -> &str {
    match name {
        "read_symbol" => "read_file",
        "patch_symbol" => "patch_file",
        _ => name,
    }
}

/// Build a `ToolOutput` message with the display name and arguments.
pub fn tool_output_msg(ptc: &PendingToolCall, output: String) -> ServerMessage {
    ServerMessage::ToolOutput {
        tool_call_id: ptc.tool_call.id.clone(),
        tool_name: tool_display_name(&ptc.tool_call.name).to_string(),
        tool_args: ptc.tool_call.arguments.clone(),
        output,
    }
}

/// Truncate tool output preserving head and tail.
pub fn truncate_tool_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let total_lines = output.bytes().filter(|&b| b == b'\n').count() + 1;
    let head_budget = max_chars * 60 / 100;
    let tail_budget = max_chars * 30 / 100;

    let mut head_end = 0;
    let mut head_lines = 0;
    for line in output.lines() {
        let next = head_end + line.len() + 1;
        if next > head_budget {
            break;
        }
        head_end = next;
        head_lines += 1;
    }

    let bytes = output.as_bytes();
    let mut tail_start = output.len();
    let mut tail_lines = 0;
    let mut pos = output.len();
    while pos > 0 {
        let line_end = pos;
        pos = if pos > 0 {
            bytes[..pos - 1]
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(0)
        } else {
            0
        };
        let line_len = line_end - pos + 1;
        if (output.len() - pos) + line_len > tail_budget {
            break;
        }
        tail_start = pos;
        tail_lines += 1;
        if pos == 0 {
            break;
        }
    }

    let head = &output[..head_end];
    let tail = &output[tail_start..];

    format!(
        "{head}\n[… truncated — {total_lines} lines total, showing first {head_lines} and last {tail_lines} …]\n{tail}",
    )
}

// ---------------------------------------------------------------------------
// js_eval — sandboxed JavaScript execution via boa_engine
// ---------------------------------------------------------------------------

async fn execute_js_eval(ptc: &PendingToolCall) -> String {
    let code = ptc.tool_call.arguments["code"]
        .as_str()
        .unwrap_or("")
        .to_string();

    if code.trim().is_empty() {
        return "Error: code must not be empty".to_string();
    }

    // Run boa in a blocking thread with a timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            use boa_engine::{Context, Source};

            let mut context = Context::default();
            match context.eval(Source::from_bytes(&code)) {
                Ok(value) => {
                    // Convert the result to a display string
                    value
                        .to_string(&mut context)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_else(|e| format!("Error converting result: {e}"))
                }
                Err(err) => {
                    format!("Error: {err}")
                }
            }
        }),
    )
    .await;

    match result {
        Ok(Ok(output)) => output,
        Ok(Err(join_err)) => format!("Error: JS execution panicked: {join_err}"),
        Err(_timeout) => "Error: JS execution timed out (5 second limit)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// git_commit
// ---------------------------------------------------------------------------

async fn execute_git_commit(ptc: &PendingToolCall) -> String {
    let message = match ptc.tool_call.arguments["message"].as_str() {
        Some(m) if !m.is_empty() => m,
        _ => return "Error: git_commit requires a non-empty 'message' argument.".to_string(),
    };

    // Stage all changes
    let add_out = match Command::new("git")
        .args(["add", "-A"])
        .current_dir(&ptc.cwd)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => return format!("Error running git add: {e}"),
    };
    if !add_out.status.success() {
        let stderr = String::from_utf8_lossy(&add_out.stderr).trim().to_string();
        return format!("Error: git add -A failed: {stderr}");
    }

    // Append co-author trailer
    let full_message = format!("{message}\n\nCo-authored-by: Bear <applegrew+bear@gmail.com>");

    // Commit
    let commit_out = match Command::new("git")
        .args(["commit", "-m", &full_message])
        .current_dir(&ptc.cwd)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => return format!("Error running git commit: {e}"),
    };

    let stdout = String::from_utf8_lossy(&commit_out.stdout)
        .trim()
        .to_string();
    let stderr = String::from_utf8_lossy(&commit_out.stderr)
        .trim()
        .to_string();

    if commit_out.status.success() {
        if stderr.is_empty() {
            stdout
        } else {
            format!("{stdout}\n{stderr}")
        }
    } else {
        let combined = if stdout.is_empty() {
            stderr
        } else {
            format!("{stdout}\n{stderr}")
        };
        format!("Error: git commit failed: {combined}")
    }
}

// ---------------------------------------------------------------------------
// js_script_save / js_script_list / js_script — reusable workspace scripts
// ---------------------------------------------------------------------------

async fn execute_js_script_save(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let name = match ptc.tool_call.arguments["name"].as_str() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "Error: js_script_save requires a 'name' argument.".to_string(),
    };

    // Validate name: [a-z0-9_-]+ only
    let name_re = regex::Regex::new(r"^[a-z0-9_-]+$").unwrap();
    if !name_re.is_match(&name) {
        return "Error: script name must match [a-z0-9_-]+ (lowercase, digits, hyphens, underscores only).".to_string();
    }

    let description = ptc.tool_call.arguments["description"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let code = match ptc.tool_call.arguments["code"].as_str() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return "Error: js_script_save requires a 'code' argument.".to_string(),
    };

    let args: Vec<crate::workspace::ScriptArg> = ptc.tool_call.arguments["args"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let name = v["name"].as_str()?.to_string();
                    let description = v["description"].as_str().unwrap_or("").to_string();
                    Some(crate::workspace::ScriptArg { name, description })
                })
                .collect()
        })
        .unwrap_or_default();

    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };

    let script = crate::workspace::SavedScript {
        name: name.clone(),
        description,
        args,
        code,
    };

    match ctx.save_script(&cwd, &script).await {
        Ok(()) => format!("Script '{name}' saved to .bear/scripts/{name}.json"),
        Err(e) => format!("Error saving script: {e}"),
    }
}

async fn execute_js_script_list(ctx: &dyn ToolContext, session_id: Uuid) -> String {
    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };

    let scripts = ctx.list_scripts(&cwd).await;
    if scripts.is_empty() {
        return "No saved scripts in this workspace.".to_string();
    }

    let mut out = String::new();
    for s in &scripts {
        out.push_str(&format!("### {}\n", s.name));
        if !s.description.is_empty() {
            out.push_str(&format!("{}\n", s.description));
        }
        if !s.args.is_empty() {
            out.push_str("Arguments:\n");
            for a in &s.args {
                if a.description.is_empty() {
                    out.push_str(&format!("  - {}\n", a.name));
                } else {
                    out.push_str(&format!("  - {}: {}\n", a.name, a.description));
                }
            }
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

async fn execute_js_script(
    ctx: &dyn ToolContext,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let name = match ptc.tool_call.arguments["name"].as_str() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "Error: js_script requires a 'name' argument.".to_string(),
    };

    let cwd = match ctx.get_session_cwd(session_id).await {
        Some(c) => c,
        None => return "Error: session not found".to_string(),
    };

    let script = match ctx.load_script(&cwd, &name).await {
        Ok(s) => s,
        Err(e) => return format!("Error: {e}"),
    };

    // Build argument injection preamble
    let args_obj = &ptc.tool_call.arguments["args"];
    let mut preamble = String::new();
    for arg_def in &script.args {
        let val = &args_obj[&arg_def.name];
        if val.is_null() {
            // Inject undefined if not provided
            preamble.push_str(&format!("const {} = undefined;\n", arg_def.name));
        } else if let Some(s) = val.as_str() {
            // String value — JSON-encode to get proper escaping
            preamble.push_str(&format!(
                "const {} = {};\n",
                arg_def.name,
                serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s))
            ));
        } else {
            // Number, bool, object, array — use JSON representation
            preamble.push_str(&format!("const {} = {};\n", arg_def.name, val));
        }
    }

    let full_code = format!("{preamble}{}", script.code);

    // Reuse the same boa execution logic as js_eval
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            use boa_engine::{Context, Source};

            let mut context = Context::default();
            match context.eval(Source::from_bytes(&full_code)) {
                Ok(value) => value
                    .to_string(&mut context)
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_else(|e| format!("Error converting result: {e}")),
                Err(err) => format!("Error: {err}"),
            }
        }),
    )
    .await;

    match result {
        Ok(Ok(output)) => output,
        Ok(Err(join_err)) => format!("Error: JS execution panicked: {join_err}"),
        Err(_timeout) => "Error: JS execution timed out (5 second limit)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// ToolCallFilter — strips tool-call markup from streamed LLM chunks
// ---------------------------------------------------------------------------

/// Stateful filter that strips tool-call markup from streamed LLM chunks so
/// the client never sees raw tool-call JSON.
#[derive(Default)]
pub struct ToolCallFilter {
    inside: bool,
    close_tag: String,
    buf: String,
}

/// Check if `tag_name` looks like a tool-call open tag.
pub fn is_tool_tag(tag_name: &str) -> bool {
    tag_name == "TOOL_CALL"
        || (tag_name.contains('_')
            && tag_name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_'))
}

impl ToolCallFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a new chunk and return the text that should be shown to the user.
    pub fn feed(&mut self, chunk: &str) -> String {
        self.buf.push_str(chunk);
        let mut output = String::new();

        loop {
            if self.inside {
                if let Some(pos) = self.buf.find(self.close_tag.as_str()) {
                    self.buf = self.buf[pos + self.close_tag.len()..].to_string();
                    self.inside = false;
                    self.close_tag.clear();
                    continue;
                }
                let keep = self.close_tag.len() - 1;
                if self.buf.len() > keep {
                    self.buf = self.buf[self.buf.len() - keep..].to_string();
                }
                break;
            } else {
                let Some(bracket) = self.buf.find('[') else {
                    output.push_str(&self.buf);
                    self.buf.clear();
                    break;
                };

                let after_bracket = &self.buf[bracket + 1..];

                // Handle malformed [TOOL_CALL{ (missing ] after TOOL_CALL)
                if after_bracket.starts_with("TOOL_CALL{")
                    || after_bracket.starts_with("TOOL_CALL {")
                {
                    output.push_str(&self.buf[..bracket]);
                    self.close_tag = "[/TOOL_CALL]".to_string();
                    self.buf = after_bracket["TOOL_CALL".len()..].to_string();
                    self.inside = true;
                    continue;
                }

                let Some(close_bracket) = after_bracket.find(']') else {
                    output.push_str(&self.buf[..bracket]);
                    self.buf = self.buf[bracket..].to_string();
                    break;
                };

                let tag_name = &after_bracket[..close_bracket];
                if is_tool_tag(tag_name) {
                    output.push_str(&self.buf[..bracket]);
                    self.close_tag = format!("[/{tag_name}]");
                    self.buf = self.buf[bracket + 1 + close_bracket + 1..].to_string();
                    self.inside = true;
                    continue;
                } else {
                    let end = bracket + 1 + close_bracket + 1;
                    output.push_str(&self.buf[..end]);
                    self.buf = self.buf[end..].to_string();
                    continue;
                }
            }
        }

        output
    }

    /// Flush any remaining buffered text (call when streaming is done).
    pub fn flush(&mut self) -> String {
        if self.inside {
            self.buf.clear();
            String::new()
        } else {
            std::mem::take(&mut self.buf)
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-approved tools list
// ---------------------------------------------------------------------------

/// Tools that are auto-executed without user confirmation.
pub const AUTO_APPROVED_TOOLS: &[&str] = &[
    "todo_write",
    "todo_read",
    "plan_save",
    "plan_read",
    "plan_update",
    "web_fetch",
    "web_search",
    "lsp_diagnostics",
    "lsp_hover",
    "lsp_references",
    "lsp_symbols",
    "js_eval",
];

/// Tools that subagents are allowed to use (read-only).
pub const SUBAGENT_ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "search_text",
    "web_fetch",
    "web_search",
    "lsp_diagnostics",
    "lsp_hover",
    "lsp_references",
    "lsp_symbols",
    "read_symbol",
    "js_eval",
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PendingToolCall, ToolCall};

    fn make_js_eval_ptc(code: &str) -> PendingToolCall {
        PendingToolCall {
            tool_call: ToolCall {
                id: "test".to_string(),
                name: "js_eval".to_string(),
                arguments: serde_json::json!({ "code": code }),
            },
            cwd: "/tmp".to_string(),
        }
    }

    #[tokio::test]
    async fn js_eval_arithmetic() {
        let ptc = make_js_eval_ptc("1 + 2 * 3");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "7");
    }

    #[tokio::test]
    async fn js_eval_string_operations() {
        let ptc = make_js_eval_ptc("'hello'.toUpperCase()");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "HELLO");
    }

    #[tokio::test]
    async fn js_eval_json_stringify() {
        let ptc = make_js_eval_ptc("JSON.stringify({a: 1, b: [2, 3]})");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, r#"{"a":1,"b":[2,3]}"#);
    }

    #[tokio::test]
    async fn js_eval_json_parse() {
        let ptc = make_js_eval_ptc(r#"JSON.parse('{"x": 42}').x"#);
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "42");
    }

    #[tokio::test]
    async fn js_eval_array_methods() {
        let ptc = make_js_eval_ptc("[3,1,4,1,5].sort().join(',')");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "1,1,3,4,5");
    }

    #[tokio::test]
    async fn js_eval_math_functions() {
        let ptc = make_js_eval_ptc("Math.sqrt(144)");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "12");
    }

    #[tokio::test]
    async fn js_eval_multiline_code() {
        let ptc = make_js_eval_ptc("let sum = 0;\nfor (let i = 1; i <= 10; i++) sum += i;\nsum");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "55");
    }

    #[tokio::test]
    async fn js_eval_syntax_error() {
        let ptc = make_js_eval_ptc("function {{{");
        let result = execute_js_eval(&ptc).await;
        assert!(
            result.starts_with("Error:"),
            "Expected error, got: {result}"
        );
    }

    #[tokio::test]
    async fn js_eval_runtime_error() {
        let ptc = make_js_eval_ptc("undefinedVariable.property");
        let result = execute_js_eval(&ptc).await;
        assert!(
            result.starts_with("Error:"),
            "Expected error, got: {result}"
        );
    }

    #[tokio::test]
    async fn js_eval_empty_code() {
        let ptc = make_js_eval_ptc("");
        let result = execute_js_eval(&ptc).await;
        assert!(result.contains("must not be empty"));
    }

    #[tokio::test]
    async fn js_eval_undefined_result() {
        let ptc = make_js_eval_ptc("undefined");
        let result = execute_js_eval(&ptc).await;
        assert_eq!(result, "undefined");
    }

    #[tokio::test]
    async fn js_eval_no_filesystem_access() {
        // require() doesn't exist in boa — should error
        let ptc = make_js_eval_ptc("require('fs')");
        let result = execute_js_eval(&ptc).await;
        assert!(
            result.starts_with("Error:"),
            "Expected error, got: {result}"
        );
    }

    #[tokio::test]
    async fn js_eval_no_process_access() {
        let ptc = make_js_eval_ptc("process.exit(1)");
        let result = execute_js_eval(&ptc).await;
        assert!(
            result.starts_with("Error:"),
            "Expected error, got: {result}"
        );
    }
}
