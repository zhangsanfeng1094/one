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