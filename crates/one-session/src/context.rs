use one_core::message::AgentMessage;

use crate::entries::SessionEntry;

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub provider: Option<String>,
    pub model_id: Option<String>,
    pub thinking_level: Option<String>,
}

pub fn build_context_entries(entries: &[SessionEntry], leaf_id: &str) -> Vec<SessionEntry> {
    let path = walk_to_root(entries, leaf_id);
    let mut result = Vec::new();

    if let Some(compaction_idx) = path.iter().position(|entry| {
        matches!(entry, SessionEntry::Compaction { .. })
    }) {
        let compaction = &path[compaction_idx];
        result.push(compaction.clone());

        if let SessionEntry::Compaction {
            first_kept_entry_id,
            ..
        } = compaction
        {
            let kept_start = path
                .iter()
                .position(|entry| entry.id() == first_kept_entry_id)
                .unwrap_or(0);
            result.extend(path[kept_start..=compaction_idx].iter().cloned());
            result.extend(path[compaction_idx + 1..].iter().cloned());
            return result;
        }
    }

    path
}

pub fn build_session_context(entries: &[SessionEntry], leaf_id: &str) -> SessionContext {
    let active = build_context_entries(entries, leaf_id);
    let mut provider = None;
    let mut model_id = None;
    let mut thinking_level = None;
    let mut messages = Vec::new();

    let full_path = walk_to_root(entries, leaf_id);
    for entry in full_path.iter().rev() {
        match entry {
            SessionEntry::ModelChange {
                provider: p,
                model_id: m,
                ..
            } => {
                provider.get_or_insert_with(|| p.clone());
                model_id.get_or_insert_with(|| m.clone());
            }
            SessionEntry::ThinkingLevelChange {
                thinking_level: level,
                ..
            } => {
                thinking_level.get_or_insert_with(|| level.clone());
            }
            _ => {}
        }
    }

    for entry in active {
        match entry {
            SessionEntry::Message { message, .. } => messages.push(message),
            SessionEntry::Compaction { summary, .. } => {
                messages.push(AgentMessage::assistant_text(
                    "system",
                    "compaction",
                    format!("[Compaction summary]\n{summary}"),
                ));
            }
            SessionEntry::BranchSummary { summary, .. } => {
                messages.push(AgentMessage::assistant_text(
                    "system",
                    "branch",
                    format!("[Branch summary]\n{summary}"),
                ));
            }
            SessionEntry::CustomMessage { content, .. } => {
                if let Some(text) = content.as_str() {
                    messages.push(AgentMessage::user_text(text));
                }
            }
            _ => {}
        }
    }

    SessionContext {
        messages,
        provider,
        model_id,
        thinking_level,
    }
}

fn walk_to_root(entries: &[SessionEntry], leaf_id: &str) -> Vec<SessionEntry> {
    let index: std::collections::HashMap<&str, &SessionEntry> =
        entries.iter().map(|entry| (entry.id(), entry)).collect();

    let mut path = Vec::new();
    let mut current = leaf_id;
    while let Some(entry) = index.get(current) {
        path.push((*entry).clone());
        current = entry.parent_id().unwrap_or("");
        if entry.parent_id().is_none() {
            break;
        }
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::{new_entry_base, SessionEntry};
    use one_core::message::AgentMessage;

    #[test]
    fn builds_message_context_from_leaf() {
        let root = new_entry_base(None);
        let child = new_entry_base(Some(root.id.clone()));
        let entries = vec![
            SessionEntry::Message {
                base: root.clone(),
                message: AgentMessage::user_text("hello"),
            },
            SessionEntry::Message {
                base: child.clone(),
                message: AgentMessage::assistant_text("mock", "v1", "hi"),
            },
        ];

        let ctx = build_session_context(&entries, &child.id);
        assert_eq!(ctx.messages.len(), 2);
    }
}