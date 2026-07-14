use chrono::{DateTime, Utc};
use one_core::message::AgentMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SESSION_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionLine {
    Session(SessionHeader),
    Entry(SessionEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type", default = "default_session_type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        #[serde(flatten)]
        base: EntryBase,
        message: AgentMessage,
    },
    Compaction {
        #[serde(flatten)]
        base: EntryBase,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    BranchSummary {
        #[serde(flatten)]
        base: EntryBase,
        from_id: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    Custom {
        #[serde(flatten)]
        base: EntryBase,
        custom_type: String,
        data: Value,
    },
    CustomMessage {
        #[serde(flatten)]
        base: EntryBase,
        custom_type: String,
        content: Value,
        display: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    Label {
        #[serde(flatten)]
        base: EntryBase,
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    ModelChange {
        #[serde(flatten)]
        base: EntryBase,
        provider: String,
        model_id: String,
    },
    ThinkingLevelChange {
        #[serde(flatten)]
        base: EntryBase,
        thinking_level: String,
    },
    SessionInfo {
        #[serde(flatten)]
        base: EntryBase,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: DateTime<Utc>,
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            SessionEntry::Message { base, .. }
            | SessionEntry::Compaction { base, .. }
            | SessionEntry::BranchSummary { base, .. }
            | SessionEntry::Custom { base, .. }
            | SessionEntry::CustomMessage { base, .. }
            | SessionEntry::Label { base, .. }
            | SessionEntry::ModelChange { base, .. }
            | SessionEntry::ThinkingLevelChange { base, .. }
            | SessionEntry::SessionInfo { base, .. } => &base.id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionEntry::Message { base, .. }
            | SessionEntry::Compaction { base, .. }
            | SessionEntry::BranchSummary { base, .. }
            | SessionEntry::Custom { base, .. }
            | SessionEntry::CustomMessage { base, .. }
            | SessionEntry::Label { base, .. }
            | SessionEntry::ModelChange { base, .. }
            | SessionEntry::ThinkingLevelChange { base, .. }
            | SessionEntry::SessionInfo { base, .. } => base.parent_id.as_deref(),
        }
    }
}

pub fn new_entry_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn default_session_type() -> String {
    "session".to_string()
}

pub fn new_session_header(cwd: &str) -> SessionHeader {
    SessionHeader {
        kind: "session".to_string(),
        version: SESSION_VERSION,
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        cwd: cwd.to_string(),
        parent_session: None,
    }
}

pub fn new_entry_base(parent_id: Option<String>) -> EntryBase {
    EntryBase {
        id: new_entry_id(),
        parent_id,
        timestamp: Utc::now(),
    }
}