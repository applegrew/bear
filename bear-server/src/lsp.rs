use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, DocumentSymbol, DocumentSymbolParams,
    DocumentSymbolResponse, Hover, HoverParams, InitializeParams, InitializedParams, Location,
    PartialResultParams, Position, PublishDiagnosticsParams, ReferenceContext, ReferenceParams,
    SymbolInformation, SymbolKind, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use serde::{Deserialize, Serialize};

/// Convert an absolute file path to a file:// URI.
fn file_path_to_uri(path: &str) -> Result<Uri, String> {
    if !path.starts_with('/') {
        return Err(format!("Path must be absolute: {path}"));
    }
    let uri_str = format!("file://{path}");
    uri_str
        .parse::<Uri>()
        .map_err(|e| format!("Invalid URI: {e}"))
}

/// Convert a file:// URI string back to a file path.
fn uri_to_file_path(uri: &Uri) -> String {
    let s = uri.as_str();
    s.strip_prefix("file://").unwrap_or(s).to_string()
}
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

/// Map file extension to (language_id, default LSP command).
/// Users can override via BEAR_LSP_<LANG> env vars.
fn detect_language(path: &str) -> Option<(&'static str, &'static str)> {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => Some(("rust", "rust-analyzer")),
        "ts" | "tsx" | "js" | "jsx" => Some(("typescript", "typescript-language-server --stdio")),
        "py" => Some(("python", "pyright-langserver --stdio")),
        "go" => Some(("go", "gopls")),
        "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" => Some(("c", "clangd")),
        "java" => Some(("java", "jdtls")),
        "zig" => Some(("zig", "zls")),
        _ => None,
    }
}

/// Get the LSP server command for a language, checking env override first.
fn lsp_command_for_lang(lang_id: &str, default_cmd: &str) -> String {
    let env_key = format!("BEAR_LSP_{}", lang_id.to_uppercase());
    std::env::var(&env_key).unwrap_or_else(|_| default_cmd.to_string())
}

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<u64>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// LspClient — communicates with a single LSP server process
// ---------------------------------------------------------------------------

struct LspClient {
    #[allow(dead_code)]
    child: Child,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    next_id: Arc<Mutex<u64>>,
    /// Pending request responses: id -> oneshot sender
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>>,
    /// Cached diagnostics per file URI
    diagnostics: Arc<RwLock<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
    /// Set of file URIs we've already sent didOpen for
    opened_files: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl LspClient {
    /// Spawn an LSP server and perform the initialize handshake.
    async fn spawn(cmd_line: &str, workspace_root: &str) -> Result<Self, String> {
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();
        if parts.is_empty() {
            return Err("Empty LSP command".to_string());
        }

        let mut cmd = Command::new(parts[0]);
        for arg in &parts[1..] {
            cmd.arg(arg);
        }

        // Ensure --stdio for servers that need it
        if !cmd_line.contains("--stdio")
            && !cmd_line.contains("rust-analyzer")
            && !cmd_line.contains("clangd")
            && !cmd_line.contains("gopls")
            && !cmd_line.contains("zls")
        {
            cmd.arg("--stdio");
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(workspace_root);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn LSP server '{}': {}", parts[0], e))?;

        let stdin = child.stdin.take().ok_or("No stdin")?;
        let stdout = child.stdout.take().ok_or("No stdout")?;

        let pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Arc<RwLock<HashMap<String, Vec<lsp_types::Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn reader task
        let pending_clone = pending.clone();
        let diag_clone = diagnostics.clone();
        tokio::spawn(async move {
            if let Err(e) = reader_loop(stdout, pending_clone, diag_clone).await {
                tracing::warn!("LSP reader loop ended: {e}");
            }
        });

        let client = Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            next_id: Arc::new(Mutex::new(1)),
            pending,
            diagnostics,
            opened_files: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };

        // Send initialize
        let root_uri = file_path_to_uri(workspace_root)?;

        #[allow(deprecated)]
        let init_params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri.clone()),
            root_path: Some(workspace_root.to_string()),
            capabilities: ClientCapabilities {
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    hover: Some(lsp_types::HoverClientCapabilities {
                        dynamic_registration: Some(false),
                        content_format: Some(vec![lsp_types::MarkupKind::PlainText]),
                    }),
                    references: Some(lsp_types::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        symbol_kind: None,
                        hierarchical_document_symbol_support: Some(true),
                        tag_support: None,
                    }),
                    publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                        related_information: Some(true),
                        tag_support: None,
                        version_support: Some(false),
                        code_description_support: Some(false),
                        data_support: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            initialization_options: None,
            trace: None,
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: root_uri,
                name: workspace_root
                    .rsplit('/')
                    .next()
                    .unwrap_or("workspace")
                    .to_string(),
            }]),
            client_info: Some(lsp_types::ClientInfo {
                name: "bear".to_string(),
                version: Some("0.1.6".to_string()),
            }),
            locale: None,
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        };

        let init_value = serde_json::to_value(&init_params)
            .map_err(|e| format!("Failed to serialize init params: {e}"))?;

        let resp = client.send_request("initialize", Some(init_value)).await?;

        if let Some(err) = resp.error {
            return Err(format!(
                "LSP initialize error: {} ({})",
                err.message, err.code
            ));
        }

        // Send initialized notification
        let initialized_value = serde_json::to_value(&InitializedParams {})
            .map_err(|e| format!("Failed to serialize initialized params: {e}"))?;
        client
            .send_notification("initialized", Some(initialized_value))
            .await?;

        Ok(client)
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<JsonRpcResponse, String> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let body = serde_json::to_string(&req).map_err(|e| format!("JSON serialize: {e}"))?;
        let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(msg.as_bytes())
                .await
                .map_err(|e| format!("Write to LSP stdin: {e}"))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("Flush LSP stdin: {e}"))?;
        }

        // Wait with timeout
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err("LSP response channel closed".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("LSP request timed out (30s)".to_string())
            }
        }
    }

    async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<(), String> {
        let body = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(Value::Null),
        }))
        .map_err(|e| format!("JSON serialize: {e}"))?;

        let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("Write to LSP stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Flush LSP stdin: {e}"))?;
        Ok(())
    }

    /// Ensure a file is opened in the LSP server (sends didOpen if needed).
    async fn ensure_file_open(&self, file_path: &str) -> Result<(), String> {
        let uri = file_path_to_uri(file_path)?;
        let uri_str = uri.as_str().to_string();

        let mut opened = self.opened_files.lock().await;
        if opened.contains(&uri_str) {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(file_path)
            .await
            .map_err(|e| format!("Failed to read {file_path}: {e}"))?;

        let lang_id = detect_language(file_path)
            .map(|(l, _)| l)
            .unwrap_or("plaintext");

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: lang_id.to_string(),
                version: 1,
                text: content,
            },
        };

        let value = serde_json::to_value(&params).map_err(|e| format!("Serialize didOpen: {e}"))?;
        self.send_notification("textDocument/didOpen", Some(value))
            .await?;

        opened.insert(uri_str);

        // Give the server a moment to process and publish diagnostics
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok(())
    }

    /// Get cached diagnostics for a file.
    async fn get_diagnostics(&self, file_path: &str) -> Vec<lsp_types::Diagnostic> {
        let uri = match file_path_to_uri(file_path) {
            Ok(u) => u.as_str().to_string(),
            Err(_) => return Vec::new(),
        };
        let diags = self.diagnostics.read().await;
        diags.get(&uri).cloned().unwrap_or_default()
    }

    /// Request hover information at a position.
    async fn hover(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<Hover>, String> {
        let uri = file_path_to_uri(file_path)?;

        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        };

        let value = serde_json::to_value(&params).map_err(|e| format!("Serialize: {e}"))?;
        let resp = self.send_request("textDocument/hover", Some(value)).await?;

        if let Some(err) = resp.error {
            return Err(format!("Hover error: {} ({})", err.message, err.code));
        }

        match resp.result {
            Some(Value::Null) | None => Ok(None),
            Some(val) => {
                let hover: Hover = serde_json::from_value(val)
                    .map_err(|e| format!("Parse hover response: {e}"))?;
                Ok(Some(hover))
            }
        }
    }

    /// Request references for a symbol at a position.
    async fn references(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>, String> {
        let uri = file_path_to_uri(file_path)?;

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            context: ReferenceContext {
                include_declaration: true,
            },
        };

        let value = serde_json::to_value(&params).map_err(|e| format!("Serialize: {e}"))?;
        let resp = self
            .send_request("textDocument/references", Some(value))
            .await?;

        if let Some(err) = resp.error {
            return Err(format!("References error: {} ({})", err.message, err.code));
        }

        match resp.result {
            Some(Value::Null) | None => Ok(Vec::new()),
            Some(val) => {
                let locs: Vec<Location> = serde_json::from_value(val)
                    .map_err(|e| format!("Parse references response: {e}"))?;
                Ok(locs)
            }
        }
    }

    /// Request document symbols for a file.
    async fn document_symbols(&self, file_path: &str) -> Result<DocumentSymbolResponse, String> {
        let uri = file_path_to_uri(file_path)?;

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let value = serde_json::to_value(&params).map_err(|e| format!("Serialize: {e}"))?;
        let resp = self
            .send_request("textDocument/documentSymbol", Some(value))
            .await?;

        if let Some(err) = resp.error {
            return Err(format!(
                "Document symbols error: {} ({})",
                err.message, err.code
            ));
        }

        match resp.result {
            Some(Value::Null) | None => Ok(DocumentSymbolResponse::Flat(Vec::new())),
            Some(val) => {
                // Try hierarchical first, then flat
                if let Ok(nested) = serde_json::from_value::<Vec<DocumentSymbol>>(val.clone()) {
                    Ok(DocumentSymbolResponse::Nested(nested))
                } else if let Ok(flat) = serde_json::from_value::<Vec<SymbolInformation>>(val) {
                    Ok(DocumentSymbolResponse::Flat(flat))
                } else {
                    Ok(DocumentSymbolResponse::Flat(Vec::new()))
                }
            }
        }
    }
}

/// Reader loop: reads JSON-RPC messages from LSP stdout, dispatches responses
/// and handles notifications (especially publishDiagnostics).
async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>>,
    diagnostics: Arc<RwLock<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
) -> Result<(), String> {
    let mut reader = BufReader::new(stdout);
    let mut header_buf = String::new();

    loop {
        // Read headers
        let mut content_length: Option<usize> = None;
        header_buf.clear();

        loop {
            header_buf.clear();
            let bytes_read = reader
                .read_line(&mut header_buf)
                .await
                .map_err(|e| format!("Read header: {e}"))?;
            if bytes_read == 0 {
                return Err("LSP stdout closed".to_string());
            }

            let line = header_buf.trim();
            if line.is_empty() {
                break; // End of headers
            }

            if let Some(val) = line.strip_prefix("Content-Length: ") {
                content_length = val.trim().parse().ok();
            }
        }

        let content_length = match content_length {
            Some(len) => len,
            None => continue, // No content-length header, skip
        };

        // Read body
        let mut body = vec![0u8; content_length];
        reader
            .read_exact(&mut body)
            .await
            .map_err(|e| format!("Read body: {e}"))?;

        let body_str = String::from_utf8_lossy(&body);
        let msg: JsonRpcResponse = match serde_json::from_str(&body_str) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("Failed to parse LSP message: {e}");
                continue;
            }
        };

        // Handle notification
        if msg.id.is_none() {
            if let Some(method) = &msg.method {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = &msg.params {
                        if let Ok(diag_params) =
                            serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
                        {
                            let uri = diag_params.uri.to_string();
                            let mut diags = diagnostics.write().await;
                            diags.insert(uri, diag_params.diagnostics);
                        }
                    }
                }
            }
            continue;
        }

        // Handle response
        if let Some(id) = msg.id {
            let mut pend = pending.lock().await;
            if let Some(tx) = pend.remove(&id) {
                let _ = tx.send(msg);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LSP availability check
// ---------------------------------------------------------------------------

/// Check if any known LSP server binary is available on $PATH.
/// This is used at startup to decide whether to include LSP tools in the
/// system prompt sent to the LLM.
pub fn any_lsp_server_available() -> bool {
    // All known (language_id, default_command) pairs
    let known: &[(&str, &str)] = &[
        ("rust", "rust-analyzer"),
        ("typescript", "typescript-language-server"),
        ("python", "pyright-langserver"),
        ("go", "gopls"),
        ("c", "clangd"),
        ("java", "jdtls"),
        ("zig", "zls"),
    ];

    for (lang_id, default_cmd) in known {
        let cmd = lsp_command_for_lang(lang_id, default_cmd);
        // Take just the binary name (first word, before any args)
        let binary = cmd.split_whitespace().next().unwrap_or(&cmd);
        if which_binary(binary) {
            return true;
        }
    }
    false
}

/// Check if a binary exists on $PATH using `which`.
fn which_binary(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// LspManager — owns all LSP server instances
// ---------------------------------------------------------------------------

pub struct LspManager {
    /// Map from language_id to LspClient
    servers: Mutex<HashMap<String, Arc<LspClient>>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
        }
    }

    /// Get or spawn an LSP client for the given file path.
    async fn get_client(
        &self,
        file_path: &str,
        workspace_root: &str,
    ) -> Result<Arc<LspClient>, String> {
        let (lang_id, default_cmd) = detect_language(file_path)
            .ok_or_else(|| format!("No LSP server configured for file: {file_path}"))?;

        let mut servers = self.servers.lock().await;
        if let Some(client) = servers.get(lang_id) {
            return Ok(client.clone());
        }

        let cmd = lsp_command_for_lang(lang_id, default_cmd);
        tracing::info!("Spawning LSP server for {lang_id}: {cmd}");

        let client = Arc::new(LspClient::spawn(&cmd, workspace_root).await?);
        servers.insert(lang_id.to_string(), client.clone());
        Ok(client)
    }

    // -- Public tool methods --------------------------------------------------

    /// Get diagnostics for a file.
    pub async fn diagnostics(
        &self,
        file_path: &str,
        workspace_root: &str,
    ) -> Result<String, String> {
        let client = self.get_client(file_path, workspace_root).await?;
        client.ensure_file_open(file_path).await?;

        // Wait a bit more for diagnostics to arrive after didOpen
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let diags = client.get_diagnostics(file_path).await;
        if diags.is_empty() {
            return Ok("No diagnostics (errors/warnings) for this file.".to_string());
        }

        let mut lines = Vec::new();
        for d in &diags {
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => "ERROR",
                Some(lsp_types::DiagnosticSeverity::WARNING) => "WARNING",
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => "INFO",
                Some(lsp_types::DiagnosticSeverity::HINT) => "HINT",
                _ => "DIAG",
            };
            let line = d.range.start.line + 1;
            let col = d.range.start.character + 1;
            lines.push(format!(
                "{}:{}:{}: {} {}",
                file_path, line, col, severity, d.message
            ));
        }
        Ok(lines.join("\n"))
    }

    /// Get hover info at a position.
    pub async fn hover(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<String, String> {
        let client = self.get_client(file_path, workspace_root).await?;
        client.ensure_file_open(file_path).await?;

        match client.hover(file_path, line, character).await? {
            None => Ok("No hover information available at this position.".to_string()),
            Some(hover) => {
                let text = match hover.contents {
                    lsp_types::HoverContents::Scalar(mc) => markup_content_to_string(mc),
                    lsp_types::HoverContents::Array(arr) => arr
                        .into_iter()
                        .map(markup_content_to_string)
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    lsp_types::HoverContents::Markup(mc) => mc.value,
                };
                if text.is_empty() {
                    Ok("No hover information available at this position.".to_string())
                } else {
                    Ok(text)
                }
            }
        }
    }

    /// Find references to a symbol at a position.
    pub async fn references(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<String, String> {
        let client = self.get_client(file_path, workspace_root).await?;
        client.ensure_file_open(file_path).await?;

        let locs = client.references(file_path, line, character).await?;
        if locs.is_empty() {
            return Ok("No references found.".to_string());
        }

        let mut lines = Vec::new();
        for loc in &locs {
            let path = uri_to_file_path(&loc.uri);
            let line = loc.range.start.line + 1;
            let col = loc.range.start.character + 1;
            lines.push(format!("{path}:{line}:{col}"));
        }
        Ok(format!(
            "{} references found:\n{}",
            locs.len(),
            lines.join("\n")
        ))
    }

    /// Find the line range of a named symbol in a file.
    /// Returns (start_line_0indexed, end_line_0indexed) or an error message.
    pub async fn find_symbol_range(
        &self,
        file_path: &str,
        symbol_name: &str,
        workspace_root: &str,
    ) -> Result<(u32, u32), String> {
        let client = self.get_client(file_path, workspace_root).await?;
        client.ensure_file_open(file_path).await?;

        let response = client.document_symbols(file_path).await?;
        match response {
            DocumentSymbolResponse::Nested(symbols) => {
                if let Some((start, end)) = find_in_nested(&symbols, symbol_name) {
                    Ok((start, end))
                } else {
                    let available = list_nested_names(&symbols, 0);
                    Err(format!(
                        "Symbol '{}' not found in {}.\nAvailable symbols:\n{}",
                        symbol_name, file_path, available
                    ))
                }
            }
            DocumentSymbolResponse::Flat(symbols) => {
                for sym in &symbols {
                    if sym.name == symbol_name {
                        return Ok((sym.location.range.start.line, sym.location.range.end.line));
                    }
                }
                let available: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
                Err(format!(
                    "Symbol '{}' not found in {}.\nAvailable symbols: {}",
                    symbol_name,
                    file_path,
                    available.join(", ")
                ))
            }
        }
    }

    /// Get document symbols (outline) for a file.
    pub async fn symbols(&self, file_path: &str, workspace_root: &str) -> Result<String, String> {
        let client = self.get_client(file_path, workspace_root).await?;
        client.ensure_file_open(file_path).await?;

        let response = client.document_symbols(file_path).await?;
        match response {
            DocumentSymbolResponse::Nested(symbols) => {
                if symbols.is_empty() {
                    return Ok("No symbols found in this file.".to_string());
                }
                let mut lines = Vec::new();
                format_nested_symbols(&symbols, 0, &mut lines);
                Ok(lines.join("\n"))
            }
            DocumentSymbolResponse::Flat(symbols) => {
                if symbols.is_empty() {
                    return Ok("No symbols found in this file.".to_string());
                }
                let mut lines = Vec::new();
                for sym in &symbols {
                    let kind = symbol_kind_str(sym.kind);
                    let line = sym.location.range.start.line + 1;
                    lines.push(format!("{kind} {} (line {line})", sym.name));
                }
                Ok(lines.join("\n"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn markup_content_to_string(mc: lsp_types::MarkedString) -> String {
    match mc {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

/// Recursively search nested document symbols for a name, return (start_line, end_line).
fn find_in_nested(symbols: &[DocumentSymbol], name: &str) -> Option<(u32, u32)> {
    for sym in symbols {
        if sym.name == name {
            return Some((sym.range.start.line, sym.range.end.line));
        }
        if let Some(children) = &sym.children {
            if let Some(found) = find_in_nested(children, name) {
                return Some(found);
            }
        }
    }
    None
}

/// List all symbol names from nested symbols for error messages.
fn list_nested_names(symbols: &[DocumentSymbol], depth: usize) -> String {
    let mut out = Vec::new();
    for sym in symbols {
        let indent = "  ".repeat(depth);
        let kind = symbol_kind_str(sym.kind);
        let start = sym.range.start.line + 1;
        let end = sym.range.end.line + 1;
        out.push(format!("{indent}{kind} {} (lines {start}-{end})", sym.name));
        if let Some(children) = &sym.children {
            out.push(list_nested_names(children, depth + 1));
        }
    }
    out.join("\n")
}

fn format_nested_symbols(symbols: &[DocumentSymbol], depth: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    for sym in symbols {
        let kind = symbol_kind_str(sym.kind);
        let start = sym.range.start.line + 1;
        let end = sym.range.end.line + 1;
        let detail = sym
            .detail
            .as_ref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        out.push(format!(
            "{indent}{kind} {} (lines {start}-{end}){detail}",
            sym.name
        ));
        if let Some(children) = &sym.children {
            format_nested_symbols(children, depth + 1, out);
        }
    }
}

fn symbol_kind_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "mod",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "fn",
        SymbolKind::VARIABLE => "var",
        SymbolKind::CONSTANT => "const",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "bool",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum_member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type_param",
        _ => "symbol",
    }
}
