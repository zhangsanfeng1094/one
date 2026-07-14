use std::path::{Path, PathBuf};

use chrono::Utc;
use one_core::message::AgentMessage;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::context::{build_context_entries, build_session_context, SessionContext};
use crate::entries::{new_entry_base, new_session_header, SessionEntry, SessionHeader};
use crate::error::{Result, SessionError};
use crate::paths::session_dir_for_cwd;

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub modified: chrono::DateTime<Utc>,
}

pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    leaf_id: Option<String>,
    file: Option<PathBuf>,
    cwd: PathBuf,
}

impl SessionManager {
    pub fn in_memory(cwd: impl AsRef<Path>) -> Self {
        let cwd = cwd.as_ref().to_path_buf();
        Self {
            header: new_session_header(&cwd.to_string_lossy()),
            entries: Vec::new(),
            leaf_id: None,
            file: None,
            cwd,
        }
    }

    pub async fn create(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let dir = session_dir_for_cwd(&cwd);
        fs::create_dir_all(&dir).await?;

        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let file = dir.join(format!("{timestamp}_{}.jsonl", uuid::Uuid::new_v4().simple()));

        let mut manager = Self::in_memory(&cwd);
        manager.file = Some(file.clone());
        manager.persist_header().await?;
        Ok(manager)
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let raw = fs::read_to_string(&path).await?;
        let content = crate::migrate::migrate_jsonl(&raw).unwrap_or(raw);
        Self::from_jsonl(&content).map(|mut manager| {
            manager.file = Some(path);
            manager
        })
    }

    pub async fn continue_recent(cwd: impl AsRef<Path>) -> Result<Self> {
        let sessions = Self::list(cwd.as_ref()).await?;
        let latest = sessions
            .into_iter()
            .max_by_key(|session| session.modified)
            .ok_or(SessionError::NoSessions)?;
        Self::open(latest.path).await
    }

    pub async fn list(cwd: impl AsRef<Path>) -> Result<Vec<SessionInfo>> {
        let dir = session_dir_for_cwd(cwd.as_ref());
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(manager) = Self::open(&path).await {
                sessions.push(SessionInfo {
                    path,
                    id: manager.header.id.clone(),
                    cwd: manager.header.cwd.clone(),
                    name: manager.session_name(),
                    modified: manager
                        .entries
                        .last()
                        .map(|entry| match entry {
                            SessionEntry::Message { base, .. }
                            | SessionEntry::Compaction { base, .. }
                            | SessionEntry::BranchSummary { base, .. }
                            | SessionEntry::Custom { base, .. }
                            | SessionEntry::CustomMessage { base, .. }
                            | SessionEntry::Label { base, .. }
                            | SessionEntry::ModelChange { base, .. }
                            | SessionEntry::ThinkingLevelChange { base, .. }
                            | SessionEntry::SessionInfo { base, .. } => base.timestamp,
                        })
                        .unwrap_or(manager.header.timestamp),
                });
            }
        }
        Ok(sessions)
    }

    pub fn from_jsonl(content: &str) -> Result<Self> {
        let mut header = None;
        let mut entries = Vec::new();
        let mut leaf_id = None;

        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)?;
            if value.get("type").and_then(|v| v.as_str()) == Some("session") {
                header = Some(serde_json::from_value::<SessionHeader>(value)?);
                continue;
            }
            let entry: SessionEntry = serde_json::from_value(value)?;
            leaf_id = Some(entry.id().to_string());
            entries.push(entry);
        }

        let header = header.ok_or_else(|| SessionError::InvalidFormat("missing header".into()))?;
        let cwd = PathBuf::from(&header.cwd);

        Ok(Self {
            header,
            entries,
            leaf_id,
            file: None,
            cwd,
        })
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn get_leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.entries.iter().find(|entry| entry.id() == id)
    }

    pub fn get_children(&self, parent_id: &str) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.parent_id() == Some(parent_id))
            .collect()
    }

    pub fn branch(&mut self, entry_id: &str) -> Result<()> {
        if self.get_entry(entry_id).is_none() {
            return Err(SessionError::EntryNotFound(entry_id.to_string()));
        }
        self.leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn session_name(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::SessionInfo { name, .. } => Some(name.clone()),
                _ => None,
            })
    }

    pub fn build_context_entries(&self) -> Vec<SessionEntry> {
        let leaf = match &self.leaf_id {
            Some(id) => id.as_str(),
            None => return Vec::new(),
        };
        build_context_entries(&self.entries, leaf)
    }

    pub fn build_session_context(&self) -> SessionContext {
        let leaf = self.leaf_id.as_deref().unwrap_or("");
        if leaf.is_empty() {
            return SessionContext {
                messages: Vec::new(),
                provider: None,
                model_id: None,
                thinking_level: None,
            };
        }
        build_session_context(&self.entries, leaf)
    }

    pub fn session_file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn is_persisted(&self) -> bool {
        self.file.is_some()
    }

    pub async fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        let base = new_entry_base(self.leaf_id.clone());
        let id = base.id.clone();
        let entry = SessionEntry::Message { base, message };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.append_entry(self.entries.last().unwrap()).await?;
        Ok(id)
    }

    pub async fn append_compaction(
        &mut self,
        summary: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: u64,
    ) -> Result<String> {
        let base = new_entry_base(self.leaf_id.clone());
        let id = base.id.clone();
        let entry = SessionEntry::Compaction {
            base,
            summary: summary.into(),
            first_kept_entry_id: first_kept_entry_id.into(),
            tokens_before,
            details: None,
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.append_entry(self.entries.last().unwrap()).await?;
        Ok(id)
    }

    pub async fn append_model_change(
        &mut self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<String> {
        let base = new_entry_base(self.leaf_id.clone());
        let id = base.id.clone();
        let entry = SessionEntry::ModelChange {
            base,
            provider: provider.into(),
            model_id: model_id.into(),
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.append_entry(self.entries.last().unwrap()).await?;
        Ok(id)
    }

    pub async fn append_custom(
        &mut self,
        custom_type: impl Into<String>,
        data: serde_json::Value,
    ) -> Result<String> {
        let base = new_entry_base(self.leaf_id.clone());
        let id = base.id.clone();
        let entry = SessionEntry::Custom {
            base,
            custom_type: custom_type.into(),
            data,
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.append_entry(self.entries.last().unwrap()).await?;
        Ok(id)
    }

    pub async fn append_session_info(&mut self, name: impl Into<String>) -> Result<String> {
        let base = new_entry_base(self.leaf_id.clone());
        let id = base.id.clone();
        let entry = SessionEntry::SessionInfo {
            base,
            name: name.into(),
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.append_entry(self.entries.last().unwrap()).await?;
        Ok(id)
    }

    pub async fn append_thinking_level_change(
        &mut self,
        thinking_level: impl Into<String>,
    ) -> Result<String> {
        let base = new_entry_base(self.leaf_id.clone());
        let id = base.id.clone();
        let entry = SessionEntry::ThinkingLevelChange {
            base,
            thinking_level: thinking_level.into(),
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.append_entry(self.entries.last().unwrap()).await?;
        Ok(id)
    }

    /// Message count on the active branch (for `/session` UX).
    pub fn message_count(&self) -> usize {
        self.build_session_context().messages.len()
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn load_messages_into(&self, messages: &mut Vec<AgentMessage>) {
        messages.extend(self.build_session_context().messages);
    }

    async fn persist_header(&self) -> Result<()> {
        self.write_json(&self.header).await
    }

    async fn append_entry(&self, entry: &SessionEntry) -> Result<()> {
        self.write_json(entry).await
    }

    async fn write_json<T: serde::Serialize>(&self, value: &T) -> Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let mut handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .await?;
        let json = serde_json::to_string(value)?;
        handle.write_all(json.as_bytes()).await?;
        handle.write_all(b"\n").await?;
        Ok(())
    }
}