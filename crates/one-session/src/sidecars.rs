//! Session sidecars: structured state persisted alongside the session JSONL.
//!
//! Inspired by `grok-build`, sidecars keep derived state (todos, plan mode state,
//! file hunk snapshots, and summaries) decoupled from the primary append-only message log.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Known sidecar kinds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SidecarKind {
    Summary,
    Todo,
    Plan,
    Hunks,
    Custom(String),
}

impl SidecarKind {
    pub fn extension_suffix(&self) -> String {
        match self {
            SidecarKind::Summary => "summary.json".to_string(),
            SidecarKind::Todo => "todo.json".to_string(),
            SidecarKind::Plan => "plan.json".to_string(),
            SidecarKind::Hunks => "hunks.json".to_string(),
            SidecarKind::Custom(name) => format!("{name}.json"),
        }
    }
}

/// Path to a sidecar file given the main session JSONL path.
pub fn sidecar_path_for(session_jsonl: &Path, kind: &SidecarKind) -> PathBuf {
    let parent = session_jsonl.parent().unwrap_or_else(|| Path::new("."));
    let stem = session_jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    parent.join(format!("{stem}.{}", kind.extension_suffix()))
}

/// Generic atomic write for any serializable sidecar payload.
pub fn write_sidecar_json<T: Serialize>(session_jsonl: &Path, kind: &SidecarKind, data: &T) -> std::io::Result<PathBuf> {
    let path = sidecar_path_for(session_jsonl, kind);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("{}.tmp", kind.extension_suffix()));
    std::fs::write(&tmp, json)?;
    if std::fs::rename(&tmp, &path).is_err() {
        std::fs::write(&path, serde_json::to_string_pretty(data).unwrap_or_default())?;
    }
    Ok(path)
}

/// Generic read for any deserializable sidecar payload.
pub fn read_sidecar_json<T: for<'de> Deserialize<'de>>(session_jsonl: &Path, kind: &SidecarKind) -> Option<T> {
    let path = sidecar_path_for(session_jsonl, kind);
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Async atomic write for sidecar json.
pub async fn write_sidecar_json_async<T: Serialize + Send + Sync>(
    session_jsonl: &Path,
    kind: SidecarKind,
    data: T,
) -> std::io::Result<PathBuf> {
    let path = sidecar_path_for(session_jsonl, &kind);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("{}.tmp", kind.extension_suffix()));
    tokio::fs::write(&tmp, &json).await?;
    if tokio::fs::rename(&tmp, &path).await.is_err() {
        tokio::fs::write(&path, &json).await?;
    }
    Ok(path)
}

/// Async read for sidecar json.
pub async fn read_sidecar_json_async<T: for<'de> Deserialize<'de> + Send + 'static>(
    session_jsonl: &Path,
    kind: SidecarKind,
) -> Option<T> {
    let path = sidecar_path_for(session_jsonl, &kind);
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&raw).ok()
}

// -----------------------------------------------------------------------------
// Concrete Sidecar Structures
// -----------------------------------------------------------------------------

/// Todo item representation in the session todo sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItemRecord {
    pub id: String,
    pub content: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// Structured Todo sidecar snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoSidecar {
    pub session_id: String,
    pub updated_at: DateTime<Utc>,
    pub items: Vec<TodoItemRecord>,
}

impl TodoSidecar {
    pub fn new(session_id: impl Into<String>, items: Vec<TodoItemRecord>) -> Self {
        Self {
            session_id: session_id.into(),
            updated_at: Utc::now(),
            items,
        }
    }
}

/// Structured Plan mode sidecar snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSidecar {
    pub session_id: String,
    pub updated_at: DateTime<Utc>,
    pub plan_text: String,
    pub approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
}

impl PlanSidecar {
    pub fn new(session_id: impl Into<String>, plan_text: impl Into<String>, approved: bool) -> Self {
        Self {
            session_id: session_id.into(),
            updated_at: Utc::now(),
            plan_text: plan_text.into(),
            approved,
            step_index: None,
        }
    }
}

/// File hunk snapshot entry for rewind & file state tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHunkRecord {
    pub file_path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
}

/// Prompt-indexed hunk snapshots sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkSnapshotsSidecar {
    pub session_id: String,
    pub updated_at: DateTime<Utc>,
    pub snapshots: Vec<PromptHunkSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHunkSnapshot {
    pub prompt_index: usize,
    pub entry_id: String,
    pub timestamp: DateTime<Utc>,
    pub hunks: Vec<FileHunkRecord>,
}

impl HunkSnapshotsSidecar {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            updated_at: Utc::now(),
            snapshots: Vec::new(),
        }
    }

    pub fn add_snapshot(&mut self, snapshot: PromptHunkSnapshot) {
        // Replace existing prompt_index if already present, or append
        if let Some(pos) = self.snapshots.iter().position(|s| s.prompt_index == snapshot.prompt_index) {
            self.snapshots[pos] = snapshot;
        } else {
            self.snapshots.push(snapshot);
        }
        self.updated_at = Utc::now();
    }

    pub fn truncate_after_prompt(&mut self, max_prompt_index: usize) -> Vec<PromptHunkSnapshot> {
        let (kept, removed) = self.snapshots.drain(..).partition(|s| s.prompt_index <= max_prompt_index);
        self.snapshots = kept;
        self.updated_at = Utc::now();
        removed
    }
}
