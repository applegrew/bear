// ---------------------------------------------------------------------------
// Implements bear_core::tools::{ToolContext, ToolBus} for bear-server types.
// This bridges the trait-based tool execution in bear-core with the concrete
// ServerState and BusSender types in bear-server.
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use bear_core::tools::{ToolBus, ToolContext};
use bear_core::{ServerMessage, TodoItem, UndoEntry};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::state::{BusSender, ManagedProcess, ServerState};
use bear_core::ProcessInfo;

#[async_trait]
impl ToolContext for ServerState {
    fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    fn max_tool_output_chars(&self) -> usize {
        self.config.max_tool_output_chars
    }

    fn google_api_key(&self) -> Option<&str> {
        self.config.google_api_key.as_deref()
    }

    fn google_cx(&self) -> Option<&str> {
        self.config.google_cx.as_deref()
    }

    fn brave_api_key(&self) -> Option<&str> {
        self.config.brave_api_key.as_deref()
    }

    async fn get_session_cwd(&self, session_id: Uuid) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.cwd.clone())
    }

    async fn push_undo(&self, session_id: Uuid, path: &str, previous_content: String) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.undo_stack.push(UndoEntry {
                path: path.to_string(),
                previous_content,
            });
            // Keep undo stack bounded
            if session.undo_stack.len() > 50 {
                session.undo_stack.remove(0);
            }
        }
    }

    async fn get_undo_entries(&self, session_id: Uuid, steps: usize) -> Vec<UndoEntry> {
        let mut sessions = self.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return Vec::new();
        };
        let count = steps.min(session.undo_stack.len());
        let entries: Vec<UndoEntry> = session
            .undo_stack
            .drain(session.undo_stack.len() - count..)
            .collect();
        entries
    }

    async fn set_todo_list(&self, session_id: Uuid, items: Vec<TodoItem>) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.todo_list = items;
        }
    }

    async fn get_todo_list(&self, session_id: Uuid) -> Vec<TodoItem> {
        let sessions = self.sessions.read().await;
        sessions
            .get(&session_id)
            .map(|s| s.todo_list.clone())
            .unwrap_or_default()
    }

    async fn set_session_cwd(&self, session_id: Uuid, new_cwd: String) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.info.cwd = new_cwd;
            session.info.touch();
        }
    }

    async fn register_process(
        &self,
        session_id: Uuid,
        pid: u32,
        command: String,
        stdin_tx: mpsc::Sender<String>,
    ) {
        let mut processes = self.processes.write().await;
        processes.insert(
            pid,
            ManagedProcess {
                info: ProcessInfo {
                    pid,
                    command,
                    running: true,
                },
                session_id,
                stdin_tx: Some(stdin_tx),
            },
        );
    }

    async fn mark_process_exited(&self, pid: u32) {
        let mut processes = self.processes.write().await;
        if let Some(proc) = processes.get_mut(&pid) {
            proc.info.running = false;
            proc.stdin_tx = None;
        }
    }

    async fn load_workspace_auto_approved(&self, cwd: &str) -> std::collections::HashSet<String> {
        self.workspace_store.load_auto_approved(cwd).await
    }

    async fn save_workspace_auto_approved(
        &self,
        cwd: &str,
        set: &std::collections::HashSet<String>,
    ) {
        if let Err(e) = self.workspace_store.save_auto_approved(cwd, set).await {
            tracing::warn!("failed to persist auto_approved: {e}");
        }
    }

    async fn reset_session_auto_approved(
        &self,
        session_id: Uuid,
        new_set: std::collections::HashSet<String>,
    ) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.auto_approved = new_set;
        }
    }

    async fn save_script(
        &self,
        cwd: &str,
        script: &bear_core::workspace::SavedScript,
    ) -> Result<(), String> {
        self.workspace_store.save_script(cwd, script).await
    }

    async fn load_script(
        &self,
        cwd: &str,
        name: &str,
    ) -> Result<bear_core::workspace::SavedScript, String> {
        self.workspace_store.load_script(cwd, name).await
    }

    async fn list_scripts(&self, cwd: &str) -> Vec<bear_core::workspace::SavedScript> {
        self.workspace_store.list_scripts(cwd).await
    }

    async fn save_plan(
        &self,
        cwd: &str,
        plan: &bear_core::workspace::SavedPlan,
    ) -> Result<(), String> {
        self.workspace_store.save_plan(cwd, plan).await
    }

    async fn load_plan(
        &self,
        cwd: &str,
        name: &str,
    ) -> Result<bear_core::workspace::SavedPlan, String> {
        self.workspace_store.load_plan(cwd, name).await
    }

    async fn list_plans(&self, cwd: &str) -> Vec<bear_core::workspace::SavedPlan> {
        self.workspace_store.list_plans(cwd).await
    }

    async fn delete_plan(&self, cwd: &str, name: &str) -> Result<(), String> {
        self.workspace_store.delete_plan(cwd, name).await
    }

    async fn get_current_plan(&self, session_id: uuid::Uuid) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).and_then(|s| s.current_plan.clone())
    }

    async fn set_current_plan(&self, session_id: uuid::Uuid, name: Option<String>) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.current_plan = name;
        }
    }

    async fn lsp_diagnostics(
        &self,
        file_path: &str,
        workspace_root: &str,
    ) -> Result<String, String> {
        self.lsp_manager
            .diagnostics(file_path, workspace_root)
            .await
    }

    async fn lsp_hover(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<String, String> {
        self.lsp_manager
            .hover(file_path, line, character, workspace_root)
            .await
    }

    async fn lsp_references(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<String, String> {
        self.lsp_manager
            .references(file_path, line, character, workspace_root)
            .await
    }

    async fn lsp_symbols(&self, file_path: &str, workspace_root: &str) -> Result<String, String> {
        self.lsp_manager.symbols(file_path, workspace_root).await
    }

    async fn lsp_find_symbol_range(
        &self,
        file_path: &str,
        symbol: &str,
        workspace_root: &str,
    ) -> Result<(u32, u32), String> {
        self.lsp_manager
            .find_symbol_range(file_path, symbol, workspace_root)
            .await
    }
}

#[async_trait]
impl ToolBus for BusSender {
    async fn send(&self, msg: ServerMessage) {
        self.send(msg).await;
    }
}
