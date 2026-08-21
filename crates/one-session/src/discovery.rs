//! Foreign & Global session discovery.
//!
//! Inspired by `grok-build`'s `IndexableSession` and `SessionSource` trait,
//! this module allows discovering, indexing, and filtering sessions across all
//! workspaces, detecting crashed or active sessions, and querying metadata fast.

use std::path::PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::manager::scan_session_list_info;
use crate::meta::UsageFields;
use crate::paths::session_root;
use crate::presence::{inspect_session_presence, SessionPresence};

/// A indexed session representation suitable for global listing and search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexableSession {
    pub session_id: String,
    pub cwd: String,
    pub path: PathBuf,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub modified: DateTime<Utc>,
    pub presence: SessionPresence,
    pub is_crashed: bool,
    pub usage_total: Option<UsageFields>,
    pub model: Option<String>,
}

impl IndexableSession {
    pub fn display_label(&self) -> String {
        if let Some(name) = self.name.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return name.to_string();
        }
        if let Some(preview) = self.preview.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return preview.to_string();
        }
        self.session_id.chars().take(12).collect()
    }
}

/// Abstract trait for session discovery providers.
pub trait SessionSource: Send + Sync {
    fn list_sessions(&self) -> Result<Vec<IndexableSession>>;
    fn find_sessions(&self, query: &str) -> Result<Vec<IndexableSession>>;
}

/// Global session discovery scanning the entire `~/.one/agent/sessions/` hierarchy.
#[derive(Debug, Default, Clone)]
pub struct GlobalSessionDiscovery;

impl GlobalSessionDiscovery {
    pub fn new() -> Self {
        Self
    }

    /// List all workspaces that currently have recorded sessions.
    pub fn list_all_workspace_dirs() -> Vec<PathBuf> {
        let root = session_root();
        let mut dirs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("--") && name.ends_with("--") {
                        dirs.push(entry.path());
                    }
                }
            }
        }
        dirs
    }

    /// Discover sessions across all workspaces on the host machine.
    pub fn discover_all() -> Vec<IndexableSession> {
        let workspace_dirs = Self::list_all_workspace_dirs();
        let mut all_sessions = Vec::new();

        for dir in workspace_dirs {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                        if let Some(meta) = entry.metadata().ok() {
                            let modified: DateTime<Utc> = meta.modified().unwrap_or(std::time::SystemTime::now()).into();
                            if let Some(info) = scan_session_list_info(&path, modified) {
                                let presence = inspect_session_presence(&path);
                                let is_crashed = presence.is_crashed();
                                all_sessions.push(IndexableSession {
                                    session_id: info.id,
                                    cwd: info.cwd,
                                    path: info.path,
                                    name: info.name,
                                    preview: info.preview,
                                    modified: info.modified,
                                    presence,
                                    is_crashed,
                                    usage_total: info.usage_total,
                                    model: info.model,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Sort newest first
        all_sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
        all_sessions
    }

    /// Find crashed sessions that can be recovered.
    pub fn discover_crashed() -> Vec<IndexableSession> {
        Self::discover_all().into_iter().filter(|s| s.is_crashed).collect()
    }
}

impl SessionSource for GlobalSessionDiscovery {
    fn list_sessions(&self) -> Result<Vec<IndexableSession>> {
        Ok(Self::discover_all())
    }

    fn find_sessions(&self, query: &str) -> Result<Vec<IndexableSession>> {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return Ok(Self::discover_all());
        }

        let filtered = Self::discover_all()
            .into_iter()
            .filter(|s| {
                s.session_id.to_ascii_lowercase().contains(&q)
                    || s.cwd.to_ascii_lowercase().contains(&q)
                    || s.name.as_ref().map(|n| n.to_ascii_lowercase().contains(&q)).unwrap_or(false)
                    || s.preview.as_ref().map(|p| p.to_ascii_lowercase().contains(&q)).unwrap_or(false)
            })
            .collect();

        Ok(filtered)
    }
}
