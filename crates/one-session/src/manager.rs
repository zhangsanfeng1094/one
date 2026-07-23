use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use one_core::message::AgentMessage;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::context::{
    build_context_entries, build_session_context, first_kept_entry_id, SessionContext,
};
use crate::entries::{new_entry_base, new_session_header, SessionEntry, SessionHeader};
use crate::error::{Result, SessionError};
use crate::paths::session_dir_for_cwd;

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    /// Explicit display name from a `session_info` entry (`/name`).
    pub name: Option<String>,
    /// First user message preview (Codex-style fallback when `name` is unset).
    pub preview: Option<String>,
    pub modified: chrono::DateTime<Utc>,
}

impl SessionInfo {
    /// Label for `/resume` lists and notices: named → first prompt → short id.
    pub fn display_label(&self) -> String {
        if let Some(name) = self
            .name
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return name.to_string();
        }
        if let Some(preview) = self
            .preview
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return preview.to_string();
        }
        self.id.chars().take(12).collect()
    }
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
        let file = dir.join(format!(
            "{timestamp}_{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));

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
            .next() // `list` returns newest-first
            .ok_or(SessionError::NoSessions)?;
        Self::open(latest.path).await
    }

    /// List sessions for a project cwd (newest first).
    ///
    /// **Performance:** does **not** fully open each JSONL. A prior implementation
    /// called [`Self::open`] (read + migrate + deserialize every entry) for every
    /// file — with ~180 multi-MB sessions that made `/resume` freeze the TUI for
    /// hundreds of ms to seconds. Listing only needs id / name / first-user
    /// preview / mtime, so we read a small prefix per file off the async runtime.
    pub async fn list(cwd: impl AsRef<Path>) -> Result<Vec<SessionInfo>> {
        let cwd = cwd.as_ref().to_path_buf();
        // Sync I/O in a blocking thread: many small prefix reads are faster as
        // plain `std::fs` than hundreds of async `read_line` syscalls, and keeps
        // the TUI event loop free while `/resume` loads.
        match tokio::task::spawn_blocking(move || list_sessions_sync(&cwd)).await {
            Ok(sessions) => Ok(sessions),
            Err(err) => Err(SessionError::Io(std::io::Error::other(format!(
                "list sessions join: {err}"
            )))),
        }
    }

    /// Lightweight metadata for a single session file (same fields as [`list`]).
    pub async fn list_info(path: impl AsRef<Path>) -> Option<SessionInfo> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<Utc> = t.into();
                    dt
                })
                .unwrap_or_else(Utc::now);
            scan_session_list_info(&path, modified)
        })
        .await
        .ok()
        .flatten()
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

    /// Rewind the active branch to *before* `entry_id` (parent becomes leaf).
    ///
    /// Used by Esc Esc / `/rewind`: drop the selected user prompt and everything
    /// after it from the active context, so the prompt can be re-edited.
    /// When the entry is a root message, the leaf is cleared (empty context).
    pub fn rewind_before(&mut self, entry_id: &str) -> Result<()> {
        let parent = self
            .get_entry(entry_id)
            .ok_or_else(|| SessionError::EntryNotFound(entry_id.to_string()))?
            .parent_id()
            .map(|s| s.to_string());
        self.leaf_id = parent;
        Ok(())
    }

    /// User prompts on the active branch (newest first), for the rewind menu.
    ///
    /// Each item is `(entry_id, display_text)` where `display_text` is a short
    /// single-line preview of the user message.
    pub fn user_prompts_for_rewind(&self) -> Vec<(String, String)> {
        let leaf = match &self.leaf_id {
            Some(id) => id.as_str(),
            None => return Vec::new(),
        };
        let path = build_context_entries(&self.entries, leaf);
        let mut out = Vec::new();
        for entry in path.iter().rev() {
            if let SessionEntry::Message {
                base,
                message: AgentMessage::User(user),
            } = entry
            {
                let text = user.content.as_display_text();
                let preview = first_line_preview(&text, 72);
                if !preview.is_empty() {
                    out.push((base.id.clone(), preview));
                }
            }
        }
        out
    }

    /// Full user-message text for a session entry (for restoring into the input).
    ///
    /// Prefer [`Self::user_prompt_for_edit`] when images must survive re-send —
    /// this returns display labels for images (`[image · png · NKB]`), which are
    /// **not** vision payloads if submitted again.
    pub fn user_prompt_text(&self, entry_id: &str) -> Option<String> {
        match self.get_entry(entry_id)? {
            SessionEntry::Message {
                message: AgentMessage::User(user),
                ..
            } => Some(user.content.as_display_text()),
            _ => None,
        }
    }

    /// Restore a user prompt for re-edit: input text (with `[图片.img]` chips) +
    /// real image `(mime, base64)` payloads in order.
    pub fn user_prompt_for_edit(&self, entry_id: &str) -> Option<(String, Vec<(String, String)>)> {
        match self.get_entry(entry_id)? {
            SessionEntry::Message {
                message: AgentMessage::User(user),
                ..
            } => Some(user.content.for_reedit()),
            _ => None,
        }
    }

    pub fn session_name(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::SessionInfo { name, .. } => Some(name.clone()),
            _ => None,
        })
    }

    /// First user message in file order, truncated for list labels.
    pub fn first_user_preview(&self) -> Option<String> {
        for entry in &self.entries {
            if let SessionEntry::Message {
                message: AgentMessage::User(user),
                ..
            } = entry
            {
                let text = user.content.as_display_text();
                let preview = first_line_preview(&text, 72);
                if !preview.is_empty() {
                    return Some(preview);
                }
            }
        }
        None
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

    /// Entry id for the oldest of the last `kept_count` context messages.
    ///
    /// Used when appending a compaction so resume keeps the full recent window.
    pub fn first_kept_entry_id_for_tail(&self, kept_count: usize) -> String {
        let leaf = self.leaf_id.as_deref().unwrap_or("");
        first_kept_entry_id(&self.entries, leaf, kept_count)
            .or_else(|| self.leaf_id.clone())
            .unwrap_or_else(|| "root".into())
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn load_messages_into(&self, messages: &mut Vec<AgentMessage>) {
        let mut loaded = self.build_session_context().messages;
        // If a prior turn was re-submitted as display labels only
        // (`这个是什么\n[image · png · 43KB]`), swap back the real multimodal
        // content from any entry in the file (including disconnected roots).
        rehydrate_image_placeholders(&mut loaded, &self.entries);
        messages.extend(loaded);
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

fn first_line_preview(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut out: String = line.chars().take(max_chars).collect();
    if line.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Max bytes to read from a session file when building the `/resume` list.
/// First user turns (and almost all `/name` renames) sit near the head; tool
/// dumps of multi-MB live later and must not be fully deserialized.
const LIST_SCAN_MAX_BYTES: u64 = 256 * 1024;

fn list_sessions_sync(cwd: &Path) -> Vec<SessionInfo> {
    let dir = session_dir_for_cwd(cwd);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut candidates: Vec<(PathBuf, chrono::DateTime<Utc>)> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        // prompt_history.jsonl lives in the same dir — not a session.
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "prompt_history.jsonl")
        {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                let dt: chrono::DateTime<Utc> = t.into();
                dt
            })
            .unwrap_or_else(Utc::now);
        candidates.push((path, modified));
    }

    // Newest first so `/resume` / `continue_recent` don't reverse readdir order.
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    candidates
        .into_iter()
        .filter_map(|(path, fs_modified)| scan_session_list_info(&path, fs_modified))
        .collect()
}

/// Prefix-scan a session JSONL for list metadata only — never materializes full
/// `SessionEntry` trees or multi-MB tool payloads into the agent message model.
fn scan_session_list_info(
    path: &Path,
    fs_modified: chrono::DateTime<Utc>,
) -> Option<SessionInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut limited = file.take(LIST_SCAN_MAX_BYTES);
    let mut content = String::new();
    limited.read_to_string(&mut content).ok()?;

    let mut id = String::new();
    let mut cwd = String::new();
    let mut name: Option<String> = None;
    let mut preview: Option<String> = None;
    let mut saw_header = false;
    let mut lines_read: usize = 0;

    // After we have a preview, skip heavy JSON parse unless the line might be
    // a rename (`session_info`). Cap how long we hunt for a late name.
    const NAME_HUNT_LINES: usize = 64;

    for raw in content.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines_read += 1;

        // Cheap reject after preview: only `session_info` can still change label.
        if preview.is_some() && !trimmed.contains("session_info") {
            if lines_read > NAME_HUNT_LINES {
                break;
            }
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match ty {
            "session" => {
                if let Ok(header) = serde_json::from_value::<SessionHeader>(value) {
                    id = header.id;
                    cwd = header.cwd;
                    saw_header = true;
                }
            }
            "session_info" => {
                if let Some(n) = value.get("name").and_then(|v| v.as_str()) {
                    let n = n.trim();
                    if !n.is_empty() {
                        name = Some(n.to_string());
                    }
                }
            }
            "message" if preview.is_none() => {
                if let Some(p) = preview_from_message_value(&value) {
                    preview = Some(p);
                }
            }
            _ => {}
        }

        // Fast path: identity + label material is enough for the picker.
        if saw_header && preview.is_some() && (name.is_some() || lines_read >= NAME_HUNT_LINES) {
            break;
        }
        // Empty / tool-only sessions: keep scanning a bit for any user turn.
        if saw_header && preview.is_none() && lines_read >= 256 {
            break;
        }
    }

    if !saw_header || id.is_empty() {
        return None;
    }

    Some(SessionInfo {
        path: path.to_path_buf(),
        id,
        cwd,
        name,
        preview,
        modified: fs_modified,
    })
}

/// Pull a short user-text preview from a raw message line without deserializing
/// the full `AgentMessage` (avoids allocating tool/assistant payloads).
fn preview_from_message_value(value: &serde_json::Value) -> Option<String> {
    let message = value.get("message")?;
    let role = message.get("role").and_then(|v| v.as_str())?;
    if role != "user" {
        return None;
    }
    let content = message.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let mut parts = Vec::new();
            for b in blocks {
                let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    "image" => parts.push("[image]".into()),
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => return None,
    };
    let preview = first_line_preview(&text, 72);
    if preview.is_empty() {
        None
    } else {
        Some(preview)
    }
}

/// Replace plain-text user turns that are only `as_display_text` snapshots of a
/// real multimodal turn with the original `UserContent::Blocks` (images included).
///
/// Happens when `/rewind` or prompt-history re-submitted `[image · png · NKB]`
/// labels instead of base64, or when a leaf branch only has that text form.
pub(crate) fn rehydrate_image_placeholders(
    messages: &mut [AgentMessage],
    entries: &[SessionEntry],
) {
    use std::collections::HashMap;

    let mut by_display: HashMap<String, one_core::message::UserContent> = HashMap::new();
    for entry in entries {
        if let SessionEntry::Message {
            message: AgentMessage::User(user),
            ..
        } = entry
        {
            if user.content.has_images() {
                by_display.insert(user.content.as_display_text(), user.content.clone());
            }
        }
    }
    if by_display.is_empty() {
        return;
    }

    for msg in messages.iter_mut() {
        if let AgentMessage::User(user) = msg {
            if !user.content.looks_like_image_placeholder_text() {
                continue;
            }
            let key = user.content.as_display_text();
            if let Some(real) = by_display.get(&key) {
                user.content = real.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::{new_entry_base, new_session_header, SessionEntry};
    use one_core::message::AgentMessage;

    #[test]
    fn display_label_prefers_name_then_preview_then_id() {
        let mut info = SessionInfo {
            path: PathBuf::from("/tmp/s.jsonl"),
            id: "abcdef0123456789".into(),
            cwd: "/tmp".into(),
            name: Some("  my task  ".into()),
            preview: Some("first prompt".into()),
            modified: Utc::now(),
        };
        assert_eq!(info.display_label(), "my task");

        info.name = None;
        assert_eq!(info.display_label(), "first prompt");

        info.preview = None;
        assert_eq!(info.display_label(), "abcdef012345");
    }

    #[test]
    fn rehydrate_swaps_display_label_text_for_real_image_blocks() {
        use one_core::message::{TextOrImage, UserContent, UserMessage};

        let tiny = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let (media_path, mime) =
            one_core::image::store_image_base64(tiny, Some("image/png")).unwrap();
        let real = UserContent::Blocks(vec![
            TextOrImage::Text {
                text: "这个是什么".into(),
            },
            TextOrImage::image_path(mime.clone(), media_path.display().to_string()),
        ]);
        let display = real.as_display_text();
        assert!(display.contains("[image ·"), "{display}");

        let u_real = new_entry_base(None);
        let a1 = new_entry_base(Some(u_real.id.clone()));
        // Disconnected re-submit as plain text (the bug).
        let u_bad = new_entry_base(None);
        let a2 = new_entry_base(Some(u_bad.id.clone()));

        let entries = vec![
            SessionEntry::Message {
                base: u_real,
                message: AgentMessage::User(UserMessage {
                    content: real.clone(),
                    timestamp: 1,
                }),
            },
            SessionEntry::Message {
                base: a1,
                message: AgentMessage::assistant_text("mock", "v1", "saw image"),
            },
            SessionEntry::Message {
                base: u_bad,
                message: AgentMessage::User(UserMessage {
                    content: UserContent::Text(display.clone()),
                    timestamp: 2,
                }),
            },
            SessionEntry::Message {
                base: a2.clone(),
                message: AgentMessage::assistant_text("mock", "v1", "only label"),
            },
        ];

        // Active leaf is the bad branch only.
        let mut msgs = vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Text(display),
                timestamp: 2,
            }),
            AgentMessage::assistant_text("mock", "v1", "only label"),
        ];
        rehydrate_image_placeholders(&mut msgs, &entries);
        match &msgs[0] {
            AgentMessage::User(u) => {
                assert!(u.content.has_images(), "should restore image blocks");
                assert_eq!(u.content.image_paths().len(), 1);
                assert_eq!(u.content.as_plain_text(), "这个是什么");
            }
            _ => panic!("expected user"),
        }

        // user_prompt_for_edit keeps chips + paths
        let header = new_session_header("/tmp");
        let mut jsonl = serde_json::to_string(&header).unwrap() + "\n";
        for e in &entries {
            jsonl.push_str(&serde_json::to_string(e).unwrap());
            jsonl.push('\n');
        }
        let sm = SessionManager::from_jsonl(&jsonl).unwrap();
        let real_id = entries[0].id().to_string();
        let (text, imgs) = sm.user_prompt_for_edit(&real_id).unwrap();
        assert!(
            text.contains(one_core::image::IMAGE_TOKEN) || text.contains("[图片"),
            "{text}"
        );
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].0, "image/png");
        assert!(std::path::Path::new(&imgs[0].1).is_file());
    }

    #[test]
    fn first_user_preview_from_entries() {
        let mut sm = SessionManager::in_memory("/tmp/proj");
        assert!(sm.first_user_preview().is_none());

        // Simulate append without disk: push entries + leaf like append_message.
        let base = crate::entries::new_entry_base(None);
        sm.entries.push(SessionEntry::Message {
            base: base.clone(),
            message: AgentMessage::user_text("hello\nsecond line"),
        });
        sm.leaf_id = Some(base.id);

        assert_eq!(sm.first_user_preview().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn list_is_lightweight_and_newest_first() {
        use std::time::Duration;
        use tokio::time::sleep;

        let dir = std::env::temp_dir().join(format!(
            "one-session-list-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Point session root at a temp tree via a fake cwd encoding.
        // `session_dir_for_cwd` uses ~/.one/agent/sessions — use real create API
        // under an isolated cwd path that won't collide.
        let cwd = dir.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut a = SessionManager::create(&cwd).await.unwrap();
        a.append_message(AgentMessage::user_text("older prompt"))
            .await
            .unwrap();
        sleep(Duration::from_millis(20)).await;
        let mut b = SessionManager::create(&cwd).await.unwrap();
        b.append_message(AgentMessage::user_text("newer prompt"))
            .await
            .unwrap();
        b.append_session_info("named-new").await.unwrap();

        let list = SessionManager::list(&cwd).await.unwrap();
        assert!(list.len() >= 2, "expected both sessions, got {list:?}");
        // Newest first.
        assert!(
            list[0].modified >= list[1].modified,
            "list must be newest-first: {:?}",
            list.iter().map(|s| s.modified).collect::<Vec<_>>()
        );
        // Labels come from lightweight scan.
        let labels: Vec<_> = list.iter().map(|s| s.display_label()).collect();
        assert!(
            labels.iter().any(|l| l.contains("named-new") || l.contains("newer")),
            "expected newer label, got {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("older")),
            "expected older preview, got {labels:?}"
        );

        // Direct path metadata matches list entry.
        let info = SessionManager::list_info(&list[0].path).await.unwrap();
        assert_eq!(info.id, list[0].id);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
