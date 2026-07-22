use one_core::message::AgentMessage;
use serde_json::Value;

use crate::entries::{new_entry_base, SessionEntry, SessionHeader, SESSION_VERSION};
use crate::error::{Result, SessionError};

/// Migrate legacy linear sessions (v1/v2) to v3 tree entries.
pub fn migrate_jsonl(content: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err(SessionError::InvalidFormat("empty session".into()));
    }

    let first: Value = serde_json::from_str(lines[0])?;
    let version = first.get("version").and_then(|v| v.as_u64()).unwrap_or(1);

    if version >= 3 {
        if lines.iter().skip(1).all(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v.get("id").cloned())
                .is_some()
        }) {
            return Ok(content.to_string());
        }
    }

    let mut header: SessionHeader = if first.get("type").and_then(|t| t.as_str()) == Some("session")
    {
        serde_json::from_value(first.clone())?
    } else {
        return Err(SessionError::InvalidFormat("missing session header".into()));
    };
    header.version = SESSION_VERSION;

    let mut output = serde_json::to_string(&header)?;
    output.push('\n');

    let mut parent_id: Option<String> = None;
    for line in lines.iter().skip(1) {
        let value: Value = serde_json::from_str(line)?;
        let entry_type = value
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("message");

        if entry_type == "message" {
            let message = value.get("message").cloned().ok_or_else(|| {
                SessionError::InvalidFormat("message entry missing message".into())
            })?;
            let base = new_entry_base(parent_id.clone());
            parent_id = Some(base.id.clone());
            let entry = SessionEntry::Message {
                base,
                message: serde_json::from_value::<AgentMessage>(message)?,
            };
            output.push_str(&serde_json::to_string(&entry)?);
            output.push('\n');
        } else if let Ok(entry) = serde_json::from_value::<SessionEntry>(value) {
            parent_id = Some(entry.id().to_string());
            output.push_str(&serde_json::to_string(&entry)?);
            output.push('\n');
        }
    }

    Ok(output)
}
