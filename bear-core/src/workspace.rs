use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

const BEAR_DIR: &str = ".bear";
const AUTO_APPROVED_FILE: &str = "auto_approved.json";
const SCRIPTS_DIR: &str = "scripts";
const PLANS_DIR: &str = "plans";

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedScript {
    pub name: String,
    pub description: String,
    pub args: Vec<ScriptArg>,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptArg {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPlan {
    pub name: String,
    pub title: String,
    pub steps: Vec<PlanStep>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SavedPlan {
    /// Recompute the overall plan status from step statuses.
    ///
    /// - `draft` — all steps pending
    /// - `completed` — all steps completed
    /// - `failed` — at least one failed, none in_progress
    /// - `in_progress` — at least one step in_progress
    /// - `partially_implemented` — some completed, some pending (no in_progress)
    pub fn recompute_status(&mut self) {
        let all_pending = self.steps.iter().all(|s| s.status == "pending");
        let any_in_progress = self.steps.iter().any(|s| s.status == "in_progress");
        let all_completed = self.steps.iter().all(|s| s.status == "completed");
        let any_completed = self.steps.iter().any(|s| s.status == "completed");
        let any_pending = self.steps.iter().any(|s| s.status == "pending");
        let any_failed = self.steps.iter().any(|s| s.status == "failed");

        self.status = if all_pending {
            "draft".to_string()
        } else if all_completed {
            "completed".to_string()
        } else if any_failed && !any_in_progress {
            "failed".to_string()
        } else if any_in_progress {
            "in_progress".to_string()
        } else if any_completed && any_pending {
            "partially_implemented".to_string()
        } else {
            "in_progress".to_string()
        };
    }
}

// ---------------------------------------------------------------------------
// WorkspaceStore — serialized per-directory I/O for .bear/
// ---------------------------------------------------------------------------

/// Manages `.bear/` folder I/O with per-directory write locks to prevent
/// race conditions when multiple sessions share the same working directory.
pub struct WorkspaceStore {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }
}

impl WorkspaceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or create) the write lock for a given working directory.
    async fn dir_lock(&self, cwd: &Path) -> Arc<Mutex<()>> {
        let mut map = self.locks.lock().await;
        map.entry(cwd.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Ensure `.bear/` directory exists under `cwd`.
    async fn ensure_bear_dir(cwd: &Path) -> std::io::Result<PathBuf> {
        let dir = cwd.join(BEAR_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// Ensure `.bear/scripts/` directory exists under `cwd`.
    async fn ensure_scripts_dir(cwd: &Path) -> std::io::Result<PathBuf> {
        let dir = cwd.join(BEAR_DIR).join(SCRIPTS_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// Ensure `.bear/plans/` directory exists under `cwd`.
    async fn ensure_plans_dir(cwd: &Path) -> std::io::Result<PathBuf> {
        let dir = cwd.join(BEAR_DIR).join(PLANS_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    // -----------------------------------------------------------------------
    // Auto-approved persistence
    // -----------------------------------------------------------------------

    /// Load the auto-approved set from `<cwd>/.bear/auto_approved.json`.
    /// Returns an empty set if the file doesn't exist or is malformed.
    pub async fn load_auto_approved(&self, cwd: &str) -> HashSet<String> {
        let path = Path::new(cwd).join(BEAR_DIR).join(AUTO_APPROVED_FILE);
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(d) => d,
            Err(_) => return HashSet::new(),
        };
        serde_json::from_str::<Vec<String>>(&data)
            .map(|v| v.into_iter().collect())
            .unwrap_or_default()
    }

    /// Save the auto-approved set to `<cwd>/.bear/auto_approved.json`.
    /// Creates the `.bear/` directory if it doesn't exist.
    pub async fn save_auto_approved(&self, cwd: &str, set: &HashSet<String>) -> Result<(), String> {
        let cwd_path = Path::new(cwd);
        let lock = self.dir_lock(cwd_path).await;
        let _guard = lock.lock().await;

        let bear_dir = Self::ensure_bear_dir(cwd_path)
            .await
            .map_err(|e| format!("failed to create .bear dir: {e}"))?;

        let path = bear_dir.join(AUTO_APPROVED_FILE);
        let mut items: Vec<&String> = set.iter().collect();
        items.sort();
        let json = serde_json::to_string_pretty(&items)
            .map_err(|e| format!("failed to serialize auto_approved: {e}"))?;
        tokio::fs::write(&path, json)
            .await
            .map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    // -----------------------------------------------------------------------
    // Script persistence
    // -----------------------------------------------------------------------

    /// Save a script to `<cwd>/.bear/scripts/<name>.json`.
    /// Silently overwrites if it already exists.
    pub async fn save_script(&self, cwd: &str, script: &SavedScript) -> Result<(), String> {
        let cwd_path = Path::new(cwd);
        let lock = self.dir_lock(cwd_path).await;
        let _guard = lock.lock().await;

        let scripts_dir = Self::ensure_scripts_dir(cwd_path)
            .await
            .map_err(|e| format!("failed to create .bear/scripts dir: {e}"))?;

        let path = scripts_dir.join(format!("{}.json", script.name));
        let json = serde_json::to_string_pretty(script)
            .map_err(|e| format!("failed to serialize script: {e}"))?;
        tokio::fs::write(&path, json)
            .await
            .map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    /// Load a script by name from `<cwd>/.bear/scripts/<name>.json`.
    pub async fn load_script(&self, cwd: &str, name: &str) -> Result<SavedScript, String> {
        let path = Path::new(cwd)
            .join(BEAR_DIR)
            .join(SCRIPTS_DIR)
            .join(format!("{name}.json"));
        let data = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("script '{name}' not found: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("failed to parse script '{name}': {e}"))
    }

    // -----------------------------------------------------------------------
    // Plan persistence
    // -----------------------------------------------------------------------

    /// Save a plan to `<cwd>/.bear/plans/<name>.json`.
    pub async fn save_plan(&self, cwd: &str, plan: &SavedPlan) -> Result<(), String> {
        let cwd_path = Path::new(cwd);
        let lock = self.dir_lock(cwd_path).await;
        let _guard = lock.lock().await;

        let plans_dir = Self::ensure_plans_dir(cwd_path)
            .await
            .map_err(|e| format!("failed to create .bear/plans dir: {e}"))?;

        let path = plans_dir.join(format!("{}.json", plan.name));
        let json = serde_json::to_string_pretty(plan)
            .map_err(|e| format!("failed to serialize plan: {e}"))?;
        tokio::fs::write(&path, json)
            .await
            .map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    /// Load a plan by name from `<cwd>/.bear/plans/<name>.json`.
    pub async fn load_plan(&self, cwd: &str, name: &str) -> Result<SavedPlan, String> {
        let path = Path::new(cwd)
            .join(BEAR_DIR)
            .join(PLANS_DIR)
            .join(format!("{name}.json"));
        let data = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("plan '{name}' not found: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("failed to parse plan '{name}': {e}"))
    }

    /// List all saved plans in `<cwd>/.bear/plans/`.
    pub async fn list_plans(&self, cwd: &str) -> Vec<SavedPlan> {
        let dir = Path::new(cwd).join(BEAR_DIR).join(PLANS_DIR);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut plans = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(data) = tokio::fs::read_to_string(&path).await {
                    if let Ok(plan) = serde_json::from_str::<SavedPlan>(&data) {
                        plans.push(plan);
                    }
                }
            }
        }
        plans.sort_by(|a, b| a.name.cmp(&b.name));
        plans
    }

    /// Delete a plan from `<cwd>/.bear/plans/<name>.json`.
    pub async fn delete_plan(&self, cwd: &str, name: &str) -> Result<(), String> {
        let cwd_path = Path::new(cwd);
        let lock = self.dir_lock(cwd_path).await;
        let _guard = lock.lock().await;

        let path = cwd_path
            .join(BEAR_DIR)
            .join(PLANS_DIR)
            .join(format!("{name}.json"));
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| format!("failed to delete plan '{name}': {e}"))
    }

    /// List all saved scripts in `<cwd>/.bear/scripts/`.
    pub async fn list_scripts(&self, cwd: &str) -> Vec<SavedScript> {
        let dir = Path::new(cwd).join(BEAR_DIR).join(SCRIPTS_DIR);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut scripts = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(data) = tokio::fs::read_to_string(&path).await {
                    if let Ok(script) = serde_json::from_str::<SavedScript>(&data) {
                        scripts.push(script);
                    }
                }
            }
        }
        scripts.sort_by(|a, b| a.name.cmp(&b.name));
        scripts
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn auto_approved_round_trip() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let store = WorkspaceStore::new();

        // Initially empty
        let set = store.load_auto_approved(cwd).await;
        assert!(set.is_empty());

        // Save and reload
        let mut set = HashSet::new();
        set.insert("cargo".to_string());
        set.insert("write_file".to_string());
        store.save_auto_approved(cwd, &set).await.unwrap();

        let loaded = store.load_auto_approved(cwd).await;
        assert_eq!(loaded, set);
    }

    #[tokio::test]
    async fn script_save_load_list() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let store = WorkspaceStore::new();

        let script = SavedScript {
            name: "greet".to_string(),
            description: "Greet someone".to_string(),
            args: vec![ScriptArg {
                name: "name".to_string(),
                description: "Person to greet".to_string(),
            }],
            code: "'Hello, ' + name".to_string(),
        };

        store.save_script(cwd, &script).await.unwrap();

        // Load by name
        let loaded = store.load_script(cwd, "greet").await.unwrap();
        assert_eq!(loaded.name, "greet");
        assert_eq!(loaded.code, "'Hello, ' + name");
        assert_eq!(loaded.args.len(), 1);

        // List
        let all = store.list_scripts(cwd).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "greet");
    }

    #[tokio::test]
    async fn script_overwrite() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let store = WorkspaceStore::new();

        let v1 = SavedScript {
            name: "calc".to_string(),
            description: "v1".to_string(),
            args: vec![],
            code: "1 + 1".to_string(),
        };
        store.save_script(cwd, &v1).await.unwrap();

        let v2 = SavedScript {
            name: "calc".to_string(),
            description: "v2".to_string(),
            args: vec![],
            code: "2 + 2".to_string(),
        };
        store.save_script(cwd, &v2).await.unwrap();

        let loaded = store.load_script(cwd, "calc").await.unwrap();
        assert_eq!(loaded.description, "v2");
        assert_eq!(loaded.code, "2 + 2");
    }

    #[tokio::test]
    async fn load_nonexistent_script() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let store = WorkspaceStore::new();

        let result = store.load_script(cwd, "nope").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn script_name_validation() {
        // This tests the regex we'll use in the tool, not WorkspaceStore itself
        let re = regex::Regex::new(r"^[a-z0-9_-]+$").unwrap();
        assert!(re.is_match("my-script"));
        assert!(re.is_match("calc_v2"));
        assert!(re.is_match("test123"));
        assert!(!re.is_match("My Script"));
        assert!(!re.is_match("../escape"));
        assert!(!re.is_match(""));
        assert!(!re.is_match("has space"));
    }

    #[tokio::test]
    async fn plan_save_load_list() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let store = WorkspaceStore::new();

        let plan = SavedPlan {
            name: "refactor".to_string(),
            title: "Refactor auth module".to_string(),
            steps: vec![
                PlanStep {
                    id: "1".into(),
                    description: "Read code".into(),
                    status: "completed".into(),
                    detail: None,
                },
                PlanStep {
                    id: "2".into(),
                    description: "Write tests".into(),
                    status: "pending".into(),
                    detail: None,
                },
            ],
            status: "in_progress".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        };

        store.save_plan(cwd, &plan).await.unwrap();

        let loaded = store.load_plan(cwd, "refactor").await.unwrap();
        assert_eq!(loaded.name, "refactor");
        assert_eq!(loaded.steps.len(), 2);

        let all = store.list_plans(cwd).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Refactor auth module");
    }

    #[tokio::test]
    async fn plan_delete() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let store = WorkspaceStore::new();

        let plan = SavedPlan {
            name: "temp".to_string(),
            title: "Temp plan".to_string(),
            steps: vec![],
            status: "draft".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        };
        store.save_plan(cwd, &plan).await.unwrap();
        assert!(store.load_plan(cwd, "temp").await.is_ok());

        store.delete_plan(cwd, "temp").await.unwrap();
        assert!(store.load_plan(cwd, "temp").await.is_err());
    }

    #[tokio::test]
    async fn plan_recompute_status() {
        let mut plan = SavedPlan {
            name: "t".into(),
            title: "t".into(),
            steps: vec![
                PlanStep {
                    id: "1".into(),
                    description: "a".into(),
                    status: "pending".into(),
                    detail: None,
                },
                PlanStep {
                    id: "2".into(),
                    description: "b".into(),
                    status: "pending".into(),
                    detail: None,
                },
            ],
            status: "".into(),
            created_at: "".into(),
            updated_at: "".into(),
        };
        plan.recompute_status();
        assert_eq!(plan.status, "draft");

        plan.steps[0].status = "in_progress".to_string();
        plan.recompute_status();
        assert_eq!(plan.status, "in_progress");

        plan.steps[0].status = "completed".to_string();
        plan.steps[1].status = "completed".to_string();
        plan.recompute_status();
        assert_eq!(plan.status, "completed");

        plan.steps[1].status = "failed".to_string();
        plan.recompute_status();
        assert_eq!(plan.status, "failed");

        // Simulate adding a new pending step to a completed plan
        plan.steps[0].status = "completed".to_string();
        plan.steps[1].status = "completed".to_string();
        plan.steps.push(PlanStep {
            id: "3".into(),
            description: "c".into(),
            status: "pending".into(),
            detail: None,
        });
        plan.recompute_status();
        assert_eq!(plan.status, "partially_implemented");
    }
}
