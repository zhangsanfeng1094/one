use one_core::message::AgentMessage;
use one_session::{new_entry_base, new_session_header, SessionEntry, SessionManager};

#[test]
fn branch_switches_context_path() {
    let root = new_entry_base(None);
    let branch_a = new_entry_base(Some(root.id.clone()));
    let branch_b = new_entry_base(Some(root.id.clone()));

    let header = new_session_header("/tmp");
    let entries = vec![
        SessionEntry::Message {
            base: root.clone(),
            message: AgentMessage::user_text("root"),
        },
        SessionEntry::Message {
            base: branch_a.clone(),
            message: AgentMessage::assistant_text("mock", "v1", "a"),
        },
        SessionEntry::Message {
            base: branch_b.clone(),
            message: AgentMessage::assistant_text("mock", "v1", "b"),
        },
    ];

    let mut jsonl = serde_json::to_string(&header).unwrap() + "\n";
    for entry in &entries {
        jsonl.push_str(&serde_json::to_string(entry).unwrap());
        jsonl.push('\n');
    }

    let mut manager = SessionManager::from_jsonl(&jsonl).expect("parse");
    manager.branch(&branch_a.id).expect("branch a");
    assert_eq!(manager.build_session_context().messages.len(), 2);

    manager.branch(&branch_b.id).expect("branch b");
    let ctx = manager.build_session_context();
    assert_eq!(ctx.messages.len(), 2);
    assert!(matches!(ctx.messages.last(), Some(AgentMessage::Assistant(a)) if a.content.iter().any(|b| matches!(b, one_core::message::ContentBlock::Text { text } if text == "b"))));
}

#[test]
fn rewind_before_drops_selected_prompt_and_later() {
    let u1 = new_entry_base(None);
    let a1 = new_entry_base(Some(u1.id.clone()));
    let u2 = new_entry_base(Some(a1.id.clone()));
    let a2 = new_entry_base(Some(u2.id.clone()));

    let header = new_session_header("/tmp");
    let entries = vec![
        SessionEntry::Message {
            base: u1.clone(),
            message: AgentMessage::user_text("first prompt"),
        },
        SessionEntry::Message {
            base: a1.clone(),
            message: AgentMessage::assistant_text("mock", "v1", "reply1"),
        },
        SessionEntry::Message {
            base: u2.clone(),
            message: AgentMessage::user_text("second prompt"),
        },
        SessionEntry::Message {
            base: a2.clone(),
            message: AgentMessage::assistant_text("mock", "v1", "reply2"),
        },
    ];

    let mut jsonl = serde_json::to_string(&header).unwrap() + "\n";
    for entry in &entries {
        jsonl.push_str(&serde_json::to_string(entry).unwrap());
        jsonl.push('\n');
    }

    let mut manager = SessionManager::from_jsonl(&jsonl).expect("parse");
    assert_eq!(manager.build_session_context().messages.len(), 4);

    let prompts = manager.user_prompts_for_rewind();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].0, u2.id); // newest first
    assert!(prompts[0].1.contains("second"));

    let text = manager.user_prompt_text(&u2.id).expect("text");
    assert_eq!(text, "second prompt");

    manager.rewind_before(&u2.id).expect("rewind");
    let ctx = manager.build_session_context();
    // Context is everything before u2 (u1 + a1).
    assert_eq!(ctx.messages.len(), 2);
    assert!(matches!(
        &ctx.messages[0],
        AgentMessage::User(u) if u.content.as_display_text() == "first prompt"
    ));

    // Rewind the first prompt → empty context.
    manager.rewind_before(&u1.id).expect("rewind root");
    assert!(manager.build_session_context().messages.is_empty());
    assert!(manager.get_leaf_id().is_none());
}
