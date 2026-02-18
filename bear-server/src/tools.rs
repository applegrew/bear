use std::io::BufRead;

use bear_core::{ProcessInfo, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::state::{BusSender, ManagedProcess, PendingToolCall, ServerState, UndoEntry};

// ---------------------------------------------------------------------------
// Tool call parsing from LLM output
// ---------------------------------------------------------------------------

pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

async fn execute_session_workdir(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    path: &str,
    cwd: &str,
) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "Error: path must not be empty".to_string();
    }

    let cmd = format!("cd {trimmed} && pwd");
    match Command::new("sh")
        .arg("-lc")
        .arg(cmd)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let new_cwd = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if new_cwd.is_empty() {
                return "Failed to resolve working directory.".to_string();
            }

            let updated_session = {
                let mut sessions = state.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.info.cwd = new_cwd.clone();
                    session.info.touch();
                    Some(session.info.clone())
                } else {
                    None
                }
            };

            if let Some(session) = updated_session {
                bus.send(ServerMessage::SessionInfo { session }).await;
            }
            format!("Working directory set to: {new_cwd}")
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if err.is_empty() {
                "Failed to change directory.".to_string()
            } else {
                format!("Failed to change directory: {err}")
            }
        }
        Err(err) => format!("Failed to change directory: {err}"),
    }
}

pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    // Format 1: [TOOL_CALL]{"name": "tool", "arguments": {…}}[/TOOL_CALL]
    let mut remaining = text;
    while let Some(start) = remaining.find("[TOOL_CALL]") {
        let after_tag = &remaining[start + 11..];
        if let Some(end) = after_tag.find("[/TOOL_CALL]") {
            let json_str = after_tag[..end].trim();
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                let name = val["name"].as_str().unwrap_or("").to_string();
                let arguments = val["arguments"].clone();
                if !name.is_empty() {
                    calls.push(ParsedToolCall { name, arguments });
                }
            }
            remaining = &after_tag[end + 12..];
        } else {
            break;
        }
    }

    // Format 2: [tool_name]{args}[/tool_name]  (tool name used as the tag)
    // Only try this if format 1 found nothing.
    if calls.is_empty() {
        let mut rem = text;
        while let Some(open) = rem.find('[') {
            let after = &rem[open + 1..];
            let Some(close) = after.find(']') else { break };
            let tag = &after[..close];
            // Must be snake_case with at least one underscore
            if tag.contains('_') && tag.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                let body_start = &after[close + 1..];
                let close_tag = format!("[/{tag}]");
                if let Some(end) = body_start.find(&close_tag) {
                    let body = body_start[..end].trim();
                    if let Ok(arguments) = serde_json::from_str::<serde_json::Value>(body) {
                        if arguments.is_object() {
                            calls.push(ParsedToolCall { name: tag.to_string(), arguments });
                        }
                    }
                    rem = &body_start[end + close_tag.len()..];
                    continue;
                }
            }
            // Not a tool tag or no matching close — skip past this bracket
            rem = &rem[open + 1..];
        }
    }

    calls
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

pub async fn execute_tool(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    ptc: &PendingToolCall,
) -> String {
    match ptc.tool_call.name.as_str() {
        "run_command" => {
            let cmd_str = ptc.tool_call.arguments["command"]
                .as_str()
                .unwrap_or("echo 'no command'")
                .to_string();
            execute_run_command(state, session_id, bus, &cmd_str, &ptc.cwd).await
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
            execute_session_workdir(state, session_id, bus, &path, &ptc.cwd).await
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
            let old_content = tokio::fs::read_to_string(&full_path).await.unwrap_or_default();
            push_undo(state, session_id, &full_path).await;
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
        "edit_file" => execute_edit_file(state, session_id, ptc).await,
        "patch_file" => execute_patch_file(state, session_id, ptc).await,
        "list_files" => execute_list_files(ptc).await,
        "search_text" => execute_search_text(ptc).await,
        "undo" => execute_undo(state, session_id, ptc).await,
        "todo_write" => execute_todo_write(state, session_id, ptc).await,
        "todo_read" => execute_todo_read(state, session_id).await,
        "web_fetch" => execute_web_fetch(state, ptc).await,
        "web_search" => execute_web_search(state, ptc).await,
        "lsp_diagnostics" => execute_lsp_diagnostics(state, session_id, ptc).await,
        "lsp_hover" => execute_lsp_hover(state, session_id, ptc).await,
        "lsp_references" => execute_lsp_references(state, session_id, ptc).await,
        "lsp_symbols" => execute_lsp_symbols(state, session_id, ptc).await,
        "read_symbol" => execute_read_symbol(state, session_id, ptc).await,
        "patch_symbol" => execute_patch_symbol(state, session_id, ptc).await,
        other => format!("Unknown tool: {other}"),
    }
}

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
fn validate_tool_path(path: &str, cwd: &str) -> Result<String, String> {
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
    Ok(full)
}

async fn push_undo(state: &ServerState, session_id: Uuid, full_path: &str) {
    let previous = tokio::fs::read_to_string(full_path).await.unwrap_or_default();
    let mut sessions = state.sessions.write().await;
    if let Some(session) = sessions.get_mut(&session_id) {
        session.undo_stack.push(UndoEntry {
            path: full_path.to_string(),
            previous_content: previous,
        });
    }
}

async fn execute_run_command(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    cmd_str: &str,
    cwd: &str,
) -> String {
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

    state.processes.write().await.insert(pid, ManagedProcess {
        info: proc_info.clone(),
        session_id,
        stdin_tx: Some(stdin_tx),
    });

    bus.send(ServerMessage::ProcessStarted {
        info: proc_info,
    }).await;

    let mut stdin_handle = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let processes = state.processes.clone();
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
        }).await;
        all_output.push_str(&line);
        all_output.push('\n');
    }

    let status = child.wait().await;
    let code = status.ok().and_then(|s| s.code());

    {
        let mut procs = processes.write().await;
        if let Some(p) = procs.get_mut(&pid) {
            p.info.running = false;
            p.stdin_tx = None;
        }
    }

    bus.send(ServerMessage::ProcessExited { pid, code }).await;

    if all_output.is_empty() {
        format!("Process exited with code {}", code.map(|c| c.to_string()).unwrap_or("unknown".into()))
    } else {
        all_output
    }
}

// ---------------------------------------------------------------------------
// edit_file — surgical find/replace
// ---------------------------------------------------------------------------

async fn execute_edit_file(
    state: &ServerState,
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

    push_undo(state, session_id, &full_path).await;
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
    state: &ServerState,
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
            push_undo(state, session_id, &full_path).await;
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

/// Unified diff applier with fuzzy hunk matching.
///
/// Parses `@@ -old_start,old_count +new_start,new_count @@` hunks.
/// For each hunk, extracts the expected context/removal lines and searches
/// for the best match in the original file near the claimed position.
/// This tolerates LLM-generated diffs where line numbers are slightly off.
fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
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
            if hline.starts_with('-') {
                hunk_lines.push(HunkLine::Remove(hline[1..].to_string()));
            } else if hline.starts_with('+') {
                hunk_lines.push(HunkLine::Add(hline[1..].to_string()));
            } else {
                // Context line (starts with ' ' or is bare text)
                let ctx = if hline.starts_with(' ') { &hline[1..] } else { hline };
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
        // These are the lines we expect to find in the original file.
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
        // Search within a window around the claimed position, starting from
        // orig_idx (we never go backwards).
        let claimed_0 = hunk.claimed_old_start.saturating_sub(1);
        let search_start = orig_idx;
        // Allow searching up to 200 lines beyond the claimed position
        let search_end = (claimed_0 + 200).min(orig_lines.len());
        let need = old_lines_expected.len();

        // Helper: search for hunk position using a line comparator
        let find_match = |cmp: &dyn Fn(&str, &str) -> bool| -> Option<usize> {
            let scan_from = search_start;
            let scan_to = if search_end >= need { search_end - need + 1 } else { scan_from };
            let mut best_pos: Option<usize> = None;
            let mut best_distance: usize = usize::MAX;

            for pos in scan_from..=scan_to.min(orig_lines.len().saturating_sub(need)) {
                let matches = old_lines_expected
                    .iter()
                    .enumerate()
                    .all(|(k, &expected)| {
                        pos + k < orig_lines.len() && cmp(orig_lines[pos + k], expected)
                    });
                if matches {
                    let distance = if pos >= claimed_0 {
                        pos - claimed_0
                    } else {
                        claimed_0 - pos
                    };
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
        // Pass 2: trailing-whitespace-trimmed match (LLMs often strip trailing spaces)
        let match_pos = find_match(&|a: &str, b: &str| a == b)
            .or_else(|| find_match(&|a: &str, b: &str| a.trim_end() == b.trim_end()));

        let match_pos = match match_pos {
            Some(p) => p,
            None => {
                // Find the first line that doesn't match to help diagnose
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
                                    expected.len(), expected,
                                    actual.len(), actual,
                                );
                                break;
                            }
                        } else {
                            mismatch_info = format!(
                                "\nFile has only {} lines but hunk expects {} old lines",
                                orig_lines.len(), need,
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
        // lines to preserve exact whitespace (trailing spaces, tabs, etc.)
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
fn generate_unified_diff(old: &str, new: &str, path: &str, context: usize) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Simple LCS-based diff using a DP table
    let n = old_lines.len();
    let m = new_lines.len();

    // Build LCS length table
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

    // Trace back to build edit script: 'E' = equal, 'D' = delete, 'I' = insert
    let mut edits: Vec<(char, usize, usize)> = Vec::new(); // (op, old_idx, new_idx)
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
        return String::new(); // no changes
    }

    // Group edits into hunks with context
    let mut hunks: Vec<(usize, usize)> = Vec::new(); // (start, end) indices into edits
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

    // Single hunk for simplicity (covers all changes with context)
    if let (Some(start), Some(last)) = (hunk_start, last_change) {
        let end = (last + context + 1).min(edits.len());
        hunks.push((start, end));
    }

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));

    for (start, end) in &hunks {
        // Calculate line numbers for the hunk header
        let mut old_start = 0usize;
        let mut old_count = 0usize;
        let mut new_start = 0usize;
        let mut new_count = 0usize;
        let mut first = true;

        for idx in *start..*end {
            let (op, oi, ni) = edits[idx];
            if first {
                old_start = oi + 1;
                new_start = ni + 1;
                first = false;
            }
            match op {
                'E' => { old_count += 1; new_count += 1; }
                'D' => { old_count += 1; }
                'I' => { new_count += 1; }
                _ => {}
            }
        }

        out.push_str(&format!("@@ -{},{} +{},{} @@\n", old_start, old_count, new_start, new_count));

        for idx in *start..*end {
            let (op, oi, ni) = edits[idx];
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
// list_files — directory listing with optional glob
// ---------------------------------------------------------------------------

async fn execute_list_files(ptc: &PendingToolCall) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or(".")
        .to_string();
    let pattern = ptc.tool_call.arguments["pattern"]
        .as_str()
        .map(|s| s.to_string());
    let max_depth = ptc.tool_call.arguments["max_depth"]
        .as_u64()
        .unwrap_or(3) as usize;

    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Use blocking I/O in a spawn_blocking since walkdir is sync
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
        // Single file
        let meta = fs::metadata(root_path).ok();
        let size = meta.map(|m| m.len()).unwrap_or(0);
        return format!("f {size:>8}  {root}");
    }

    let glob_pattern = pattern.and_then(|p| glob::Pattern::new(p).ok());
    let mut entries = Vec::new();
    collect_entries(root_path, root_path, 0, max_depth, &glob_pattern, &mut entries);

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

        // Skip hidden files/dirs
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
// search_text — regex grep across files
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
        search_dir(root_path, &re, &include_glob, &mut results, max_results, 0, 10);
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
        // Skip hidden
        if entry
            .file_name()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        if path.is_dir() {
            search_dir(&path, re, include, results, max_results, depth + 1, max_depth);
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
// undo — revert last file change(s)
// ---------------------------------------------------------------------------

async fn execute_undo(
    state: &ServerState,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let steps = ptc.tool_call.arguments["steps"]
        .as_u64()
        .unwrap_or(1)
        .min(10) as usize;

    let entries: Vec<UndoEntry> = {
        let mut sessions = state.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return "Error: session not found".to_string();
        };
        let available = session.undo_stack.len().min(steps);
        if available == 0 {
            return "Nothing to undo.".to_string();
        }
        let start = session.undo_stack.len() - available;
        session.undo_stack.drain(start..).rev().collect()
    };

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
    state: &ServerState,
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
        todo_list.push(crate::state::TodoItem { id, content, status, priority });
    }

    let count = todo_list.len();
    {
        let mut sessions = state.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.todo_list = todo_list;
        }
    }
    format!("Todo list updated ({count} items).")
}

async fn execute_todo_read(
    state: &ServerState,
    session_id: Uuid,
) -> String {
    let sessions = state.sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        return "Error: session not found".to_string();
    };
    if session.todo_list.is_empty() {
        return "Todo list is empty.".to_string();
    }
    let mut lines = Vec::new();
    for item in &session.todo_list {
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
        lines.push(format!("{status_icon} [{}] {}{priority_tag}", item.id, item.content));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// web_fetch / web_search
// ---------------------------------------------------------------------------

async fn execute_web_fetch(
    state: &ServerState,
    ptc: &PendingToolCall,
) -> String {
    let url = match ptc.tool_call.arguments["url"].as_str() {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => return "Error: web_fetch requires a 'url' argument.".to_string(),
    };
    let max_chars = ptc.tool_call.arguments["max_chars"]
        .as_u64()
        .unwrap_or(10_000) as usize;

    // Validate URL scheme
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return "Error: URL must start with http:// or https://".to_string();
    }

    let response = match state.http_client
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

    // Strip HTML tags to extract text content
    let text = strip_html_tags(&body);

    // Collapse whitespace runs
    let text = collapse_whitespace(&text);

    if text.len() > max_chars {
        format!("{}\n\n[... truncated at {max_chars} chars, total {} chars]", &text[..max_chars], text.len())
    } else {
        text
    }
}

/// Simple HTML tag stripper — removes tags, decodes common entities, collapses whitespace.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if !in_tag && i + 7 < len && &lower[i..i+7] == "<script" {
            in_script = true;
            in_tag = true;
            i += 1;
            continue;
        }
        if in_script && i + 9 <= len && &lower[i..i+9] == "</script>" {
            in_script = false;
            i += 9;
            continue;
        }
        if !in_tag && i + 6 < len && &lower[i..i+6] == "<style" {
            in_style = true;
            in_tag = true;
            i += 1;
            continue;
        }
        if in_style && i + 8 <= len && &lower[i..i+8] == "</style>" {
            in_style = false;
            i += 8;
            continue;
        }
        if in_script || in_style {
            i += 1;
            continue;
        }
        if chars[i] == '<' {
            in_tag = true;
            // Add newline for block elements
            if i + 2 < len {
                let next2 = &lower[i..lower.len().min(i+5)];
                if next2.starts_with("<br") || next2.starts_with("<p ")
                    || next2.starts_with("<p>") || next2.starts_with("<div")
                    || next2.starts_with("<li") || next2.starts_with("<h1")
                    || next2.starts_with("<h2") || next2.starts_with("<h3")
                    || next2.starts_with("<tr")
                {
                    result.push('\n');
                }
            }
            i += 1;
            continue;
        }
        if chars[i] == '>' {
            in_tag = false;
            i += 1;
            continue;
        }
        if !in_tag {
            // Decode common HTML entities
            if chars[i] == '&' {
                if i + 4 <= len && &html[i..i+4] == "&lt;" {
                    result.push('<');
                    i += 4;
                    continue;
                }
                if i + 4 <= len && &html[i..i+4] == "&gt;" {
                    result.push('>');
                    i += 4;
                    continue;
                }
                if i + 5 <= len && &html[i..i+5] == "&amp;" {
                    result.push('&');
                    i += 5;
                    continue;
                }
                if i + 6 <= len && &html[i..i+6] == "&quot;" {
                    result.push('"');
                    i += 6;
                    continue;
                }
                if i + 6 <= len && &html[i..i+6] == "&nbsp;" {
                    result.push(' ');
                    i += 6;
                    continue;
                }
                if i + 5 <= len && &html[i..i+5] == "&#39;" {
                    result.push('\'');
                    i += 5;
                    continue;
                }
            }
            result.push(chars[i]);
        }
        i += 1;
    }
    result
}

/// Collapse runs of whitespace into single spaces/newlines.
fn collapse_whitespace(text: &str) -> String {
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

async fn execute_web_search(
    state: &ServerState,
    ptc: &PendingToolCall,
) -> String {
    let query = match ptc.tool_call.arguments["query"].as_str() {
        Some(q) if !q.is_empty() => q.to_string(),
        _ => return "Error: web_search requires a 'query' argument.".to_string(),
    };
    let max_results = ptc.tool_call.arguments["max_results"]
        .as_u64()
        .unwrap_or(5) as usize;

    // Use DuckDuckGo HTML search (no API key needed)
    let encoded_query = urlencoding::encode(&query);
    let url = format!("https://html.duckduckgo.com/html/?q={encoded_query}");

    let response = match state.http_client
        .get(&url)
        .header("User-Agent", "Bear/1.0 (AI coding assistant)")
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => return format!("Error searching: {err}"),
    };

    let body = match response.text().await {
        Ok(t) => t,
        Err(err) => return format!("Error reading search response: {err}"),
    };

    // Parse DuckDuckGo HTML results
    parse_ddg_results(&body, max_results)
}

/// Parse DuckDuckGo HTML search results page.
/// Results are in <a class="result__a"> for title/URL and <a class="result__snippet"> for snippet.
fn parse_ddg_results(html: &str, max_results: usize) -> String {
    let mut results = Vec::new();

    // Find result blocks: <div class="result results_links results_links_deep web-result ">
    // Each result has: <a class="result__a" href="...">title</a>
    //                  <a class="result__snippet">snippet</a>
    let mut pos = 0;
    while results.len() < max_results {
        // Find next result link
        let marker = "class=\"result__a\"";
        let Some(marker_pos) = html[pos..].find(marker) else { break };
        let abs_pos = pos + marker_pos;

        // Find href
        let search_back_start = if abs_pos > 200 { abs_pos - 200 } else { 0 };
        let href = extract_href(&html[search_back_start..abs_pos + marker.len() + 200]);

        // Find title (text between > and </a>)
        let after_marker = abs_pos + marker.len();
        let title_start = html[after_marker..].find('>').map(|p| after_marker + p + 1);
        let title_end = title_start.and_then(|s| html[s..].find("</a>").map(|p| s + p));
        let title = match (title_start, title_end) {
            (Some(s), Some(e)) => strip_html_tags(&html[s..e]).trim().to_string(),
            _ => String::new(),
        };

        pos = title_end.unwrap_or(after_marker + 1);

        // Find snippet
        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(sp) = html[pos..].find(snippet_marker) {
            let sp_abs = pos + sp + snippet_marker.len();
            let sn_start = html[sp_abs..].find('>').map(|p| sp_abs + p + 1);
            let sn_end = sn_start.and_then(|s| html[s..].find("</a>").or_else(|| html[s..].find("</span>")).map(|p| s + p));
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
                if title.is_empty() { "(no title)" } else { &title },
                if href.is_empty() { "(no url)" } else { &href },
                if snippet.is_empty() { "(no snippet)" } else { &snippet },
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
            // DuckDuckGo wraps URLs in a redirect: //duckduckgo.com/l/?uddg=<encoded_url>&...
            if let Some(uddg_pos) = raw.find("uddg=") {
                let url_start = uddg_pos + 5;
                let url_end = raw[url_start..].find('&').unwrap_or(raw.len() - url_start);
                let encoded = &raw[url_start..url_start + url_end];
                return urlencoding::decode(encoded).unwrap_or_else(|_| encoded.into()).to_string();
            }
            return raw.to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// LSP tools
// ---------------------------------------------------------------------------

/// Helper to get session cwd for LSP workspace root.
async fn get_session_cwd(state: &ServerState, session_id: Uuid) -> Result<String, String> {
    let sessions = state.sessions.read().await;
    sessions
        .get(&session_id)
        .map(|s| s.info.cwd.clone())
        .ok_or_else(|| "Session not found".to_string())
}

async fn execute_lsp_diagnostics(
    state: &ServerState,
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
    let cwd = match get_session_cwd(state, session_id).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    match state.lsp_manager.diagnostics(&full_path, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

async fn execute_lsp_hover(
    state: &ServerState,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let line = ptc.tool_call.arguments["line"]
        .as_u64()
        .unwrap_or(0) as u32;
    let character = ptc.tool_call.arguments["character"]
        .as_u64()
        .unwrap_or(0) as u32;
    if path.is_empty() {
        return "Error: lsp_hover requires a 'path' argument.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match get_session_cwd(state, session_id).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    // LSP uses 0-indexed lines, but users think in 1-indexed. Accept both:
    // if line > 0, assume 1-indexed and subtract 1.
    let lsp_line = if line > 0 { line - 1 } else { 0 };
    let lsp_char = if character > 0 { character - 1 } else { 0 };
    match state.lsp_manager.hover(&full_path, lsp_line, lsp_char, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

async fn execute_lsp_references(
    state: &ServerState,
    session_id: Uuid,
    ptc: &PendingToolCall,
) -> String {
    let path = ptc.tool_call.arguments["path"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let line = ptc.tool_call.arguments["line"]
        .as_u64()
        .unwrap_or(0) as u32;
    let character = ptc.tool_call.arguments["character"]
        .as_u64()
        .unwrap_or(0) as u32;
    if path.is_empty() {
        return "Error: lsp_references requires a 'path' argument.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match get_session_cwd(state, session_id).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let lsp_line = if line > 0 { line - 1 } else { 0 };
    let lsp_char = if character > 0 { character - 1 } else { 0 };
    match state.lsp_manager.references(&full_path, lsp_line, lsp_char, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

async fn execute_lsp_symbols(
    state: &ServerState,
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
    let cwd = match get_session_cwd(state, session_id).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    match state.lsp_manager.symbols(&full_path, &cwd).await {
        Ok(result) => result,
        Err(e) => format!("LSP error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// read_symbol — read just one symbol (function, struct, etc.) from a file
// ---------------------------------------------------------------------------

async fn execute_read_symbol(
    state: &ServerState,
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
    let cwd = match get_session_cwd(state, session_id).await {
        Ok(c) => c,
        Err(e) => return e,
    };

    // Use LSP to find the symbol's line range
    let (start_line, end_line) = match state
        .lsp_manager
        .find_symbol_range(&full_path, &symbol, &cwd)
        .await
    {
        Ok(range) => range,
        Err(e) => return e,
    };

    // Read the file and extract the relevant lines
    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let start = start_line as usize;
    let end = (end_line as usize + 1).min(lines.len());

    if start >= lines.len() {
        return format!("Error: symbol range {start_line}-{end_line} is out of bounds (file has {} lines)", lines.len());
    }

    // Include a few lines of context before the symbol for imports/attributes
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
// patch_symbol — replace the body of a symbol with new content
// ---------------------------------------------------------------------------

async fn execute_patch_symbol(
    state: &ServerState,
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
        return "Error: patch_symbol requires 'path', 'symbol', and 'content' arguments.".to_string();
    }
    if new_content.is_empty() {
        return "Error: patch_symbol 'content' must not be empty.".to_string();
    }
    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match get_session_cwd(state, session_id).await {
        Ok(c) => c,
        Err(e) => return e,
    };

    // Use LSP to find the symbol's line range
    let (start_line, end_line) = match state
        .lsp_manager
        .find_symbol_range(&full_path, &symbol, &cwd)
        .await
    {
        Ok(range) => range,
        Err(e) => return e,
    };

    // Read the file
    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let start = start_line as usize;
    let end = (end_line as usize + 1).min(lines.len());

    if start >= lines.len() {
        return format!("Error: symbol range {start_line}-{end_line} is out of bounds (file has {} lines)", lines.len());
    }

    // Save undo state
    push_undo(state, session_id, &full_path).await;

    // Build the new file content
    let mut new_file = String::new();
    // Lines before the symbol
    for line in &lines[..start] {
        new_file.push_str(line);
        new_file.push('\n');
    }
    // New symbol content
    new_file.push_str(&new_content);
    if !new_content.ends_with('\n') {
        new_file.push('\n');
    }
    // Lines after the symbol
    for line in &lines[end..] {
        new_file.push_str(line);
        new_file.push('\n');
    }
    // Preserve original trailing newline behavior
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_tool_calls --------------------------------------------------

    #[test]
    fn parse_single_tool_call() {
        let text = r#"Let me read that file.
[TOOL_CALL]{"name": "read_file", "arguments": {"path": "src/main.rs"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "src/main.rs");
    }

    #[test]
    fn parse_multiple_tool_calls() {
        let text = r#"I'll read both files.
[TOOL_CALL]{"name": "read_file", "arguments": {"path": "a.rs"}}[/TOOL_CALL]
Then the second:
[TOOL_CALL]{"name": "read_file", "arguments": {"path": "b.rs"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a.rs");
        assert_eq!(calls[1].arguments["path"], "b.rs");
    }

    #[test]
    fn parse_no_tool_calls() {
        let text = "Just a normal response with no tools.";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_malformed_json_skipped() {
        let text = "[TOOL_CALL]{not valid json}[/TOOL_CALL]";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_missing_end_tag() {
        let text = r#"[TOOL_CALL]{"name": "read_file", "arguments": {"path": "a.rs"}}"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    // -- parse_tool_calls format 2: [tool_name]{args}[/tool_name] ----------

    #[test]
    fn parse_tool_name_tag_single() {
        let text = r#"Let me list files. [list_files]{"path": "."}[/list_files]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_files");
        assert_eq!(calls[0].arguments["path"], ".");
    }

    #[test]
    fn parse_tool_name_tag_multiple() {
        let text = r#"[read_file]{"path": "a.rs"}[/read_file] then [read_file]{"path": "b.rs"}[/read_file]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a.rs");
        assert_eq!(calls[1].arguments["path"], "b.rs");
    }

    #[test]
    fn parse_tool_name_tag_malformed_json() {
        let text = "[list_files]{bad json}[/list_files]";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_name_tag_missing_close() {
        let text = r#"[list_files]{"path": "."}"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_format1_takes_priority() {
        // If both formats are present, format 1 wins (format 2 only tried when format 1 finds nothing)
        let text = r#"[TOOL_CALL]{"name": "read_file", "arguments": {"path": "x"}}[/TOOL_CALL] [list_files]{"path": "."}[/list_files]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    // -- resolve_path ------------------------------------------------------

    #[test]
    fn resolve_absolute_path() {
        assert_eq!(resolve_path("/tmp/foo.txt", "/home/user"), "/tmp/foo.txt");
    }

    #[test]
    fn resolve_relative_path() {
        assert_eq!(resolve_path("src/main.rs", "/home/user/project"), "/home/user/project/src/main.rs");
    }

    #[test]
    fn resolve_parent_dir_references() {
        assert_eq!(resolve_path("../sibling/file.rs", "/home/user/project"), "/home/user/sibling/file.rs");
    }

    #[test]
    fn resolve_dot_references() {
        assert_eq!(resolve_path("./src/../src/main.rs", "/home/user/project"), "/home/user/project/src/main.rs");
    }

    #[test]
    fn resolve_multiple_parent_refs() {
        assert_eq!(resolve_path("../../file.txt", "/a/b/c"), "/a/file.txt");
    }

    // -- validate_tool_path ------------------------------------------------

    #[test]
    fn validate_relative_within_cwd() {
        let result = validate_tool_path("src/main.rs", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/user/project/src/main.rs");
    }

    #[test]
    fn validate_relative_escaping_cwd_blocked() {
        let result = validate_tool_path("../../etc/passwd", "/home/user/project");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("outside the working directory"));
    }

    #[test]
    fn validate_absolute_path_allowed() {
        // Absolute paths are allowed (user/LLM may reference /tmp, etc.)
        let result = validate_tool_path("/tmp/test.txt", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/tmp/test.txt");
    }

    #[test]
    fn validate_empty_path_rejected() {
        let result = validate_tool_path("", "/home/user/project");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn validate_dot_path_is_cwd() {
        let result = validate_tool_path(".", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/user/project");
    }

    // -- apply_unified_diff ------------------------------------------------

    #[test]
    fn diff_add_line() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -2,1 +2,2 @@\n line2\n+inserted\n line3\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "line1\nline2\ninserted\nline3\n");
    }

    #[test]
    fn diff_remove_line() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -1,3 +1,2 @@\n line1\n-line2\n line3\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "line1\nline3\n");
    }

    #[test]
    fn diff_replace_line() {
        let original = "aaa\nbbb\nccc\n";
        let diff = "@@ -1,3 +1,3 @@\n aaa\n-bbb\n+BBB\n ccc\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "aaa\nBBB\nccc\n");
    }

    #[test]
    fn diff_with_header_lines() {
        let original = "hello\nworld\n";
        let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n+universe\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "hello\nuniverse\n");
    }

    #[test]
    fn diff_off_by_few_lines() {
        // LLM claims hunk starts at line 5, but the matching context is at line 8.
        // Fuzzy matching should find it.
        let original = "a\nb\nc\nd\ne\nf\ng\ntarget_line\nh\ni\n";
        let diff = "@@ -5,3 +5,3 @@\n g\n-target_line\n+replaced_line\n h\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "a\nb\nc\nd\ne\nf\ng\nreplaced_line\nh\ni\n");
    }

    #[test]
    fn diff_multi_hunk() {
        let original = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let diff = "\
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -6,3 +6,3 @@
 f
-g
+G
 h
";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn diff_multi_hunk_off_by_one() {
        // Second hunk line number is off by 1 (claims 7, actual match at 6)
        let original = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let diff = "\
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -7,3 +7,3 @@
 f
-g
+G
 h
";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn diff_context_mismatch_returns_error() {
        // Context lines don't match anything in the file
        let original = "aaa\nbbb\nccc\n";
        let diff = "@@ -1,3 +1,3 @@\n xxx\n-yyy\n+zzz\n ccc\n";
        let result = apply_unified_diff(original, diff);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("could not find matching lines"));
    }

    #[test]
    fn diff_no_hunks_returns_error() {
        let original = "hello\n";
        let diff = "just some random text with no @@ headers";
        let result = apply_unified_diff(original, diff);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No hunks found"));
    }

    #[test]
    fn diff_large_file_off_by_many() {
        // Simulate a 100-line file where LLM is off by 10 lines
        let mut lines: Vec<String> = (1..=100).map(|i| format!("line_{i}")).collect();
        let original = lines.join("\n") + "\n";
        // Target is at line 50, but LLM claims line 40
        let diff = "@@ -40,3 +40,3 @@\n line_49\n-line_50\n+line_50_modified\n line_51\n";
        let result = apply_unified_diff(&original, diff).unwrap();
        lines[49] = "line_50_modified".to_string();
        let expected = lines.join("\n") + "\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn diff_bare_empty_lines_in_context() {
        // LLMs sometimes emit empty lines without the leading space
        let original = "fn main() {\n    println!(\"hello\");\n\n    println!(\"world\");\n}\n";
        let diff = "@@ -1,5 +1,5 @@\n fn main() {\n-    println!(\"hello\");\n+    println!(\"hi\");\n\n     println!(\"world\");\n }\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "fn main() {\n    println!(\"hi\");\n\n    println!(\"world\");\n}\n");
    }

    #[test]
    fn diff_pure_insertion_hunk() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -2,0 +3,1 @@\n+inserted\n";
        let result = apply_unified_diff(original, diff).unwrap();
        // Pure insertion at line 2 — should insert after line 2
        assert!(result.contains("inserted"));
    }

    #[test]
    fn diff_trailing_whitespace_tolerant() {
        // LLM emits context lines without trailing whitespace that exists in the file.
        // The fuzzy matcher should handle this via trailing-whitespace-trimmed fallback.
        let original = "fn foo() {  \n    bar();\n}\n";
        let diff = "@@ -1,3 +1,3 @@\n fn foo() {\n-    bar();\n+    baz();\n }\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "fn foo() {  \n    baz();\n}\n");
    }

    #[test]
    fn diff_tab_vs_spaces_mismatch() {
        // File uses tabs, LLM emits spaces in context
        let original = "fn foo() {\n\tbar();\n\tbaz();\n}\n";
        let diff = "@@ -1,4 +1,4 @@\n fn foo() {\n-    bar();\n+    qux();\n     baz();\n }\n";
        // Context uses spaces but file has tabs — should fail
        let result = apply_unified_diff(original, diff);
        assert!(result.is_err(), "Should fail on tab/space mismatch, got: {:?}", result);
    }

    #[test]
    fn diff_header_lines_not_at_start() {
        // LLM emits some text before --- / +++ headers
        let original = "aaa\nbbb\nccc\n";
        let diff = "Here is the diff:\n--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n aaa\n-bbb\n+BBB\n ccc\n";
        let result = apply_unified_diff(original, diff);
        // The "Here is the diff:" line is not --- or +++, so the initial skip loop
        // breaks immediately. Then the main loop skips it (not @@). Should still work.
        assert!(result.is_ok(), "Header not at start failed: {:?}", result.unwrap_err());
        assert_eq!(result.unwrap(), "aaa\nBBB\nccc\n");
    }

    #[test]
    fn diff_removal_line_starts_with_triple_dash() {
        // What if the LLM's diff has a line being removed that starts with "---"?
        // The initial skip loop would consume it as a header line!
        let original = "first\n--- old separator ---\nlast\n";
        let diff = "@@ -1,3 +1,3 @@\n first\n---- old separator ---\n+--- new separator ---\n last\n";
        let result = apply_unified_diff(original, diff);
        assert!(result.is_ok(), "Triple-dash removal failed: {:?}", result.unwrap_err());
        assert_eq!(result.unwrap(), "first\n--- new separator ---\nlast\n");
    }

    #[test]
    fn diff_large_hunk_exceeds_search_window() {
        // Reproduce: 120-line file, hunk at line 1 with all lines as context/remove.
        // The search_end is (claimed_0 + 200).min(orig_lines.len()) = 0 + 200 = 200,
        // but orig_lines.len() = 120, so search_end = 120.
        // need = number of old lines in hunk. If need > search_end, scan_to = scan_from.
        // This tests that the scan range handles large hunks correctly.
        let lines: Vec<String> = (1..=120).map(|i| format!("line_{i}")).collect();
        let original = lines.join("\n") + "\n";

        // Build a diff that replaces line_60 with line_60_new, with ALL other lines as context
        let mut diff = String::from("@@ -1,120 +1,120 @@\n");
        for i in 1..=120 {
            if i == 60 {
                diff.push_str(&format!("-line_{i}\n"));
                diff.push_str("+line_60_new\n");
            } else {
                diff.push_str(&format!(" line_{i}\n"));
            }
        }

        let result = apply_unified_diff(&original, &diff);
        assert!(result.is_ok(), "Large hunk failed: {:?}", result.unwrap_err());
        let patched = result.unwrap();
        assert!(patched.contains("line_60_new"));
        assert!(!patched.contains("\nline_60\n"));
    }

    #[test]
    fn diff_full_file_replacement() {
        // LLM replaces entire file content via a single hunk — all old lines are
        // removed and all new lines are added. This is the pattern that failed
        // in production: expected lines matched actual file but hunk still failed.
        let original = "\
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use log::info;
use browser::{html, css, renderer, js};
use std::fs;
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args: Vec<String> = env::args().collect();
    let url = args.get(1).map(|s| s.as_str()).unwrap_or(\"about:blank\");
    info!(\"Loading {url}\");
    Ok(())
}
";
        let diff = "\
@@ -1,18 +1,25 @@
 use tao::{
     dpi::LogicalSize,
     event::{Event, WindowEvent},
     event_loop::{ControlFlow, EventLoop},
     window::WindowBuilder,
 };
-use log::info;
-use browser::{html, css, renderer, js};
+use log::{info, debug};
+use browser::{html, css, renderer, js, layout};
 use std::fs;
 use std::env;
+use std::path::PathBuf;
 
 fn main() -> Result<(), Box<dyn std::error::Error>> {
     env_logger::init();
     let args: Vec<String> = env::args().collect();
     let url = args.get(1).map(|s| s.as_str()).unwrap_or(\"about:blank\");
     info!(\"Loading {url}\");
+    debug!(\"Starting render\");
+    let doc = html::parse(\"<html></html>\");
+    let style = css::parse(\"\");
+    let tree = layout::build(&doc, &style);
+    renderer::paint(&tree);
+    debug!(\"Render complete\");
     Ok(())
 }
";
        let result = apply_unified_diff(original, diff);
        assert!(result.is_ok(), "Full file replacement failed: {:?}", result.unwrap_err());
        let patched = result.unwrap();
        assert!(patched.contains("use log::{info, debug};"));
        assert!(patched.contains("layout::build"));
        assert!(patched.contains("debug!(\"Render complete\");"));
        assert!(!patched.contains("use log::info;"));
    }

    #[test]
    fn diff_line_content_starts_with_minus() {
        // A line in the file starts with '-', which could confuse the parser
        let original = "header\n- item one\n- item two\nfooter\n";
        let diff = "@@ -1,4 +1,4 @@\n header\n-- item one\n+- item ONE\n - item two\n footer\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "header\n- item ONE\n- item two\nfooter\n");
    }

    #[test]
    fn diff_line_content_starts_with_plus() {
        // A line in the file starts with '+', which could confuse the parser
        let original = "header\n+ item one\n+ item two\nfooter\n";
        let diff = "@@ -1,4 +1,4 @@\n header\n-+ item one\n++ item ONE\n + item two\n footer\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "header\n+ item ONE\n+ item two\nfooter\n");
    }
}
