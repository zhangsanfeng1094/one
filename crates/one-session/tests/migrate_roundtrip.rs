use one_core::message::AgentMessage;
use one_session::{migrate_jsonl, SessionManager};

#[test]
fn migrates_linear_session_to_tree() {
    let legacy = r#"{"type":"session","version":1,"id":"abc","timestamp":"2026-07-14T10:00:00Z","cwd":"/tmp"}
{"type":"message","message":{"role":"user","content":"hi"}}
"#;

    let migrated = migrate_jsonl(legacy).expect("migrate");
    assert!(migrated.contains("\"version\":3"));
    let manager = SessionManager::from_jsonl(&migrated).expect("parse");
    let ctx = manager.build_session_context();
    assert_eq!(ctx.messages.len(), 1);
    assert!(matches!(ctx.messages[0], AgentMessage::User(_)));
}
