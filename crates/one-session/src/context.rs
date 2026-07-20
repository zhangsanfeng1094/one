use one_core::message::AgentMessage;

use crate::entries::SessionEntry;

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub provider: Option<String>,
    pub model_id: Option<String>,
    pub thinking_level: Option<String>,
}

/// Entries on the active leaf path that become LLM context messages
/// (Message / Compaction / BranchSummary / string CustomMessage).
pub fn context_message_entries(entries: &[SessionEntry], leaf_id: &str) -> Vec<SessionEntry> {
    build_context_entries(entries, leaf_id)
        .into_iter()
        .filter(entry_produces_message)
        .collect()
}

fn entry_produces_message(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::Message { .. }
        | SessionEntry::Compaction { .. }
        | SessionEntry::BranchSummary { .. } => true,
        SessionEntry::CustomMessage { content, .. } => content.as_str().is_some(),
        _ => false,
    }
}

/// Entry id for the first of the last `kept_count` context messages on the leaf.
///
/// Used when writing a compaction: `first_kept_entry_id` must point at the oldest
/// message that remains after the summary, not the current leaf.
pub fn first_kept_entry_id(entries: &[SessionEntry], leaf_id: &str, kept_count: usize) -> Option<String> {
    if leaf_id.is_empty() || kept_count == 0 {
        return None;
    }
    let msg_entries = context_message_entries(entries, leaf_id);
    if msg_entries.is_empty() {
        return None;
    }
    let start = msg_entries.len().saturating_sub(kept_count);
    msg_entries.get(start).map(|e| e.id().to_string())
}

pub fn build_context_entries(entries: &[SessionEntry], leaf_id: &str) -> Vec<SessionEntry> {
    let path = walk_to_root(entries, leaf_id);
    if path.is_empty() {
        return path;
    }

    // Prefer the *latest* compaction on the path (nested / re-compact).
    let Some(compaction_idx) = path
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }))
    else {
        return path;
    };

    let SessionEntry::Compaction {
        first_kept_entry_id,
        ..
    } = &path[compaction_idx]
    else {
        return path;
    };

    let kept_start = path
        .iter()
        .position(|entry| entry.id() == first_kept_entry_id)
        .unwrap_or(0);

    // Summary once, then kept window (excluding the compaction entry itself),
    // then anything after this compaction on the branch.
    let mut result = Vec::with_capacity(path.len().saturating_sub(kept_start) + 1);
    result.push(path[compaction_idx].clone());

    let from = kept_start.min(compaction_idx);
    for entry in &path[from..compaction_idx] {
        result.push(entry.clone());
    }
    result.extend(path[compaction_idx + 1..].iter().cloned());
    result
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

    fn msg(parent: Option<String>, text: &str) -> SessionEntry {
        let base = new_entry_base(parent);
        SessionEntry::Message {
            base,
            message: AgentMessage::user_text(text),
        }
    }

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

    #[test]
    fn compaction_keeps_window_and_summary_once() {
        // m0 m1 m2 m3  then compaction with first_kept = m2
        let m0 = msg(None, "u0");
        let m0_id = m0.id().to_string();
        let m1 = msg(Some(m0_id.clone()), "u1");
        let m1_id = m1.id().to_string();
        let m2 = msg(Some(m1_id.clone()), "u2");
        let m2_id = m2.id().to_string();
        let m3 = msg(Some(m2_id.clone()), "u3");
        let m3_id = m3.id().to_string();

        let mut compact_base = new_entry_base(Some(m3_id.clone()));
        // stable id for assertions
        compact_base.id = "compact1".into();
        let compact = SessionEntry::Compaction {
            base: compact_base,
            summary: "summary of early turns".into(),
            first_kept_entry_id: m2_id.clone(),
            tokens_before: 9_000,
            details: None,
        };

        let entries = vec![m0, m1, m2, m3, compact];
        let leaf = "compact1";

        let built = build_context_entries(&entries, leaf);
        // compaction + m2 + m3  (no m0/m1, no double summary)
        assert_eq!(built.len(), 3);
        assert!(matches!(built[0], SessionEntry::Compaction { .. }));
        assert_eq!(built[1].id(), m2_id);
        assert_eq!(built[2].id(), m3_id);

        let ctx = build_session_context(&entries, leaf);
        assert_eq!(ctx.messages.len(), 3);
        // first is summary once
        let t0 = match &ctx.messages[0] {
            AgentMessage::Assistant(a) => a
                .content
                .iter()
                .find_map(|b| match b {
                    one_core::message::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
            _ => "",
        };
        assert!(t0.contains("summary of early turns"));
        assert_eq!(
            ctx.messages
                .iter()
                .filter(|m| matches!(m, AgentMessage::Assistant(_)))
                .count(),
            1
        );

        // first_kept for tail of 2 (m2,m3) before compact existed would be m2
        let pre: Vec<SessionEntry> = entries[..4].to_vec();
        let id = first_kept_entry_id(&pre, &m3_id, 2).unwrap();
        assert_eq!(id, m2_id);
    }

    #[test]
    fn wrong_leaf_as_first_kept_only_keeps_one_message() {
        // Documents the old bug: first_kept = last message id.
        let m0 = msg(None, "u0");
        let m0_id = m0.id().to_string();
        let m1 = msg(Some(m0_id), "u1");
        let m1_id = m1.id().to_string();
        let mut cbase = new_entry_base(Some(m1_id.clone()));
        cbase.id = "c".into();
        let compact = SessionEntry::Compaction {
            base: cbase,
            summary: "s".into(),
            first_kept_entry_id: m1_id.clone(), // leaf-before-compact (buggy)
            tokens_before: 1,
            details: None,
        };
        let entries = vec![m0, m1, compact];
        let built = build_context_entries(&entries, "c");
        // summary + only m1
        assert_eq!(built.len(), 2);
        assert_eq!(built[1].id(), m1_id);
    }

    #[test]
    fn latest_compaction_wins() {
        let m0 = msg(None, "a");
        let m0_id = m0.id().to_string();
        let m1 = msg(Some(m0_id.clone()), "b");
        let m1_id = m1.id().to_string();
        let mut c1 = new_entry_base(Some(m1_id.clone()));
        c1.id = "c1".into();
        let compact1 = SessionEntry::Compaction {
            base: c1,
            summary: "first".into(),
            first_kept_entry_id: m1_id.clone(),
            tokens_before: 1,
            details: None,
        };
        let m2 = msg(Some("c1".into()), "c");
        let m2_id = m2.id().to_string();
        let mut c2 = new_entry_base(Some(m2_id.clone()));
        c2.id = "c2".into();
        let compact2 = SessionEntry::Compaction {
            base: c2,
            summary: "second".into(),
            first_kept_entry_id: m2_id.clone(),
            tokens_before: 2,
            details: None,
        };
        let entries = vec![m0, m1, compact1, m2, compact2];
        let ctx = build_session_context(&entries, "c2");
        assert_eq!(ctx.messages.len(), 2); // second summary + m2
        let t0 = match &ctx.messages[0] {
            AgentMessage::Assistant(a) => a
                .content
                .iter()
                .find_map(|b| match b {
                    one_core::message::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        assert!(t0.contains("second"));
        assert!(!t0.contains("first"));
    }
}
