use std::io::BufRead;

use axum::extract::ws::WebSocket;
use bear_core::{ProcessInfo, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::state::{ManagedProcess, PendingToolCall, ServerState, UndoEntry};
use crate::ws::send_msg;

// ---------------------------------------------------------------------------
// Tool call parsing from LLM output
// ---------------------------------------------------------------------------

pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
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
    calls
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

pub async fn execute_tool(
    state: &ServerState,
    session_id: Uuid,
    socket: &mut WebSocket,
    ptc: &PendingToolCall,
) -> String {
    match ptc.tool_call.name.as_str() {
        "run_command" => {
            let cmd_str = ptc.tool_call.arguments["command"]
                .as_str()
                .unwrap_or("echo 'no command'")
                .to_string();
            execute_run_command(state, session_id, socket, &cmd_str, &ptc.cwd).await
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
    socket: &mut WebSocket,
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

    let _ = send_msg(socket, ServerMessage::ProcessStarted {
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
        let _ = send_msg(socket, ServerMessage::ProcessOutput {
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

    let _ = send_msg(socket, ServerMessage::ProcessExited { pid, code }).await;

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

    let full_path = match validate_tool_path(&path, &ptc.cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

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
        Err(err) => format!("Patch failed: {err}"),
    }
}

/// Minimal unified diff applier. Parses `@@ -old_start,old_count +new_start,new_count @@` hunks.
fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
    let orig_lines: Vec<&str> = original.lines().collect();
    let mut result_lines: Vec<String> = Vec::new();
    let mut orig_idx: usize = 0;

    let diff_lines: Vec<&str> = diff.lines().collect();
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

        // Parse @@ -old_start,old_count +new_start,new_count @@
        let hunk_header = line;
        let parts: Vec<&str> = hunk_header.split_whitespace().collect();
        if parts.len() < 3 {
            return Err(format!("Invalid hunk header: {hunk_header}"));
        }

        let old_range = parts[1].trim_start_matches('-');
        let old_start: usize = old_range
            .split(',')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        // Copy lines before this hunk
        let target = old_start.saturating_sub(1);
        while orig_idx < target && orig_idx < orig_lines.len() {
            result_lines.push(orig_lines[orig_idx].to_string());
            orig_idx += 1;
        }

        di += 1;
        // Process hunk lines
        while di < diff_lines.len() {
            let hline = diff_lines[di];
            if hline.starts_with("@@") {
                break;
            }
            if hline.starts_with('-') {
                // Remove line — skip it in original
                orig_idx += 1;
            } else if hline.starts_with('+') {
                // Add line
                result_lines.push(hline[1..].to_string());
            } else {
                // Context line (starts with ' ' or is plain)
                let ctx = if hline.starts_with(' ') { &hline[1..] } else { hline };
                result_lines.push(ctx.to_string());
                orig_idx += 1;
            }
            di += 1;
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
}
