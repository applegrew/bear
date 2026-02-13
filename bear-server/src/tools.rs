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
            let full_path = resolve_path(&path, &ptc.cwd);
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
            let full_path = resolve_path(&path, &ptc.cwd);
            push_undo(state, session_id, &full_path).await;
            if let Some(parent) = std::path::Path::new(&full_path).parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            match tokio::fs::write(&full_path, &content).await {
                Ok(()) => format!("Written {} bytes to {full_path}", content.len()),
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

fn resolve_path(path: &str, cwd: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", cwd, path)
    }
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

    let full_path = resolve_path(&path, &ptc.cwd);
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
        Ok(()) => format!("Edited {full_path} (replaced 1 occurrence)"),
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

    let full_path = resolve_path(&path, &ptc.cwd);
    let content = match tokio::fs::read_to_string(&full_path).await {
        Ok(c) => c,
        Err(err) => return format!("Error reading {full_path}: {err}"),
    };

    match apply_unified_diff(&content, &diff) {
        Ok(patched) => {
            push_undo(state, session_id, &full_path).await;
            match tokio::fs::write(&full_path, &patched).await {
                Ok(()) => format!("Patched {full_path} successfully"),
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

    let full_path = resolve_path(&path, &ptc.cwd);

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

    let full_path = resolve_path(&path, &ptc.cwd);

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
