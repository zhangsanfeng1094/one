//! Derived session sidecar (`*.summary.json`) for fast list / resume UX.
//!
//! Never the source of truth for messages — always rebuildable from the JSONL tree.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::meta::{UsageFields, META_SCHEMA};

/// Sidecar schema version (independent of session JSONL version).
pub const SUMMARY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    #[serde(default = "summary_schema")]
    pub schema: u32,
    pub id: String,
    pub cwd: String,
    /// Absolute path to the session JSONL when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_id: Option<String>,
    #[serde(default)]
    pub entry_count: usize,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_total: Option<UsageFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<UsageFields>,
    #[serde(default)]
    pub tool_call_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_used: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_index: Option<u64>,
}

fn summary_schema() -> u32 {
    SUMMARY_SCHEMA
}

/// Path of the summary sidecar next to a session JSONL file.
///
/// `foo/bar.jsonl` → `foo/bar.summary.json`
pub fn summary_path_for(session_jsonl: &Path) -> PathBuf {
    let parent = session_jsonl.parent().unwrap_or_else(|| Path::new("."));
    let stem = session_jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    parent.join(format!("{stem}.summary.json"))
}

/// Path for large system-prompt spill next to a session JSONL.
///
/// `foo/bar.jsonl` → `foo/bar.system_prompt.txt`
pub fn system_prompt_path_for(session_jsonl: &Path) -> PathBuf {
    let parent = session_jsonl.parent().unwrap_or_else(|| Path::new("."));
    let stem = session_jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    parent.join(format!("{stem}.system_prompt.txt"))
}

/// Load summary if present and schema is known; otherwise `None` (caller falls back).
pub fn load_summary(session_jsonl: &Path) -> Option<SessionSummary> {
    let path = summary_path_for(session_jsonl);
    let raw = std::fs::read_to_string(path).ok()?;
    let summary: SessionSummary = serde_json::from_str(&raw).ok()?;
    if summary.schema == 0 || summary.schema > SUMMARY_SCHEMA {
        return None;
    }
    if summary.id.is_empty() {
        return None;
    }
    Some(summary)
}

/// Atomically-ish write summary (write temp then rename when possible).
pub fn write_summary_file(session_jsonl: &Path, summary: &SessionSummary) -> std::io::Result<()> {
    let path = summary_path_for(session_jsonl);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(summary)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("summary.json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path).or_else(|_| {
        // Cross-device rename failure: fall back to direct write.
        std::fs::write(&path, serde_json::to_string_pretty(summary).unwrap_or_default())
    })?;
    Ok(())
}

/// Build a minimal [`SessionSummary`] shell (callers fill computed fields).
pub fn empty_summary(id: impl Into<String>, cwd: impl Into<String>) -> SessionSummary {
    SessionSummary {
        schema: META_SCHEMA.max(SUMMARY_SCHEMA),
        id: id.into(),
        cwd: cwd.into(),
        path: None,
        created_at: None,
        updated_at: Utc::now(),
        name: None,
        preview: None,
        leaf_id: None,
        entry_count: 0,
        message_count: 0,
        model: None,
        provider: None,
        usage_total: None,
        last_usage: None,
        tool_call_count: 0,
        tools_used: Vec::new(),
        system_prompt_hash: None,
        prompt_index: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_path_stem() {
        let p = Path::new("/tmp/sess/20260802_foo_bar.jsonl");
        assert_eq!(
            summary_path_for(p),
            PathBuf::from("/tmp/sess/20260802_foo_bar.summary.json")
        );
        assert_eq!(
            system_prompt_path_for(p),
            PathBuf::from("/tmp/sess/20260802_foo_bar.system_prompt.txt")
        );
    }

    #[test]
    fn write_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "one-summary-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("s.jsonl");
        std::fs::write(&jsonl, "{}\n").unwrap();
        let mut s = empty_summary("id-1", "/tmp");
        s.path = Some(jsonl.display().to_string());
        s.preview = Some("hello".into());
        s.usage_total = Some(UsageFields {
            input_tokens: 9,
            output_tokens: 1,
            ..Default::default()
        });
        write_summary_file(&jsonl, &s).unwrap();
        let loaded = load_summary(&jsonl).unwrap();
        assert_eq!(loaded.id, "id-1");
        assert_eq!(loaded.preview.as_deref(), Some("hello"));
        assert_eq!(loaded.usage_total.unwrap().input_tokens, 9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
