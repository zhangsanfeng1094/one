use one_core::message::AgentMessage;
use one_session::{
    read_sidecar_json, write_sidecar_json, Activity, FileHunkRecord, GlobalSessionDiscovery,
    HunkSnapshotsSidecar, PlanSidecar, PromptHunkSnapshot, RewindMode, SessionActor, SessionLock,
    SessionManager, SessionPresence, SessionSource, SidecarKind, TodoItemRecord, TodoSidecar,
};

fn unique_temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "one_session_test_{}",
        uuid::Uuid::new_v4().simple()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[tokio::test]
async fn test_rewind_marker_and_points() {
    let tmp = unique_temp_dir();
    let mut sm = SessionManager::create(&tmp).await.unwrap();

    let id1 = sm
        .append_message(AgentMessage::user_text("Prompt 1"))
        .await
        .unwrap();
    let id2 = sm
        .append_message(AgentMessage::user_text("Prompt 2"))
        .await
        .unwrap();
    let _id3 = sm
        .append_message(AgentMessage::user_text("Prompt 3"))
        .await
        .unwrap();

    let points = sm.get_rewind_points();
    assert_eq!(points.len(), 3);
    assert_eq!(points[0].preview, "Prompt 1");
    assert_eq!(points[1].preview, "Prompt 2");
    assert_eq!(points[2].preview, "Prompt 3");

    // Rewind to Prompt 2 with RewindMode::All
    let marker_id = sm
        .rewind_with_mode(
            &id2,
            RewindMode::All,
            Some(2),
            Some(vec!["src/main.rs".into()]),
        )
        .await
        .unwrap();
    assert!(!marker_id.is_empty());

    // Active leaf should now be parent of id2 (which is id1)
    assert_eq!(sm.get_leaf_id(), Some(id1.as_str()));

    // Context should only contain Prompt 1
    let ctx = sm.build_session_context();
    assert_eq!(ctx.messages.len(), 1);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_session_fork() {
    let tmp = unique_temp_dir();
    let mut sm = SessionManager::create(&tmp).await.unwrap();

    let _id1 = sm
        .append_message(AgentMessage::user_text("Step 1"))
        .await
        .unwrap();
    let _id2 = sm
        .append_message(AgentMessage::user_text("Step 2"))
        .await
        .unwrap();
    let _id3 = sm
        .append_message(AgentMessage::user_text("Step 3"))
        .await
        .unwrap();

    // Fork up to prompt index 2
    let forked = sm.fork_session(Some(2), None).await.unwrap();
    assert_ne!(forked.header().id, sm.header().id);

    let ctx = forked.build_session_context();
    assert_eq!(ctx.messages.len(), 2);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_sidecars_todo_and_plan() {
    let tmp = unique_temp_dir();
    let session_file = tmp.join("session_123.jsonl");
    std::fs::write(&session_file, "").unwrap();

    // Write Todo Sidecar
    let todo = TodoSidecar::new(
        "session_123",
        vec![
            TodoItemRecord {
                id: "1".into(),
                content: "Implement feature".into(),
                status: "in_progress".into(),
                active_form: None,
            },
            TodoItemRecord {
                id: "2".into(),
                content: "Write tests".into(),
                status: "pending".into(),
                active_form: None,
            },
        ],
    );
    write_sidecar_json(&session_file, &SidecarKind::Todo, &todo).unwrap();

    let loaded_todo: TodoSidecar = read_sidecar_json(&session_file, &SidecarKind::Todo).unwrap();
    assert_eq!(loaded_todo.items.len(), 2);
    assert_eq!(loaded_todo.items[0].content, "Implement feature");

    // Write Plan Sidecar
    let plan = PlanSidecar::new(
        "session_123",
        "# Implementation Plan\n1. Do A\n2. Do B",
        true,
    );
    write_sidecar_json(&session_file, &SidecarKind::Plan, &plan).unwrap();

    let loaded_plan: PlanSidecar = read_sidecar_json(&session_file, &SidecarKind::Plan).unwrap();
    assert!(loaded_plan.approved);
    assert!(loaded_plan.plan_text.contains("Implementation Plan"));

    // Write Hunk Snapshots Sidecar
    let mut hunks = HunkSnapshotsSidecar::new("session_123");
    hunks.add_snapshot(PromptHunkSnapshot {
        prompt_index: 1,
        entry_id: "entry_1".into(),
        timestamp: chrono::Utc::now(),
        hunks: vec![FileHunkRecord {
            file_path: "src/lib.rs".into(),
            before_hash: Some("aaa".into()),
            after_hash: Some("bbb".into()),
            patch: Some("diff --git ...".into()),
        }],
    });
    write_sidecar_json(&session_file, &SidecarKind::Hunks, &hunks).unwrap();

    let loaded_hunks: HunkSnapshotsSidecar =
        read_sidecar_json(&session_file, &SidecarKind::Hunks).unwrap();
    assert_eq!(loaded_hunks.snapshots.len(), 1);
    assert_eq!(loaded_hunks.snapshots[0].hunks[0].file_path, "src/lib.rs");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_session_presence_and_lock() {
    let tmp = unique_temp_dir();
    let session_file = tmp.join("session_test.jsonl");
    std::fs::write(&session_file, "").unwrap();

    // Acquire lock
    let mut lock = SessionLock::acquire(&session_file, "sess-1").unwrap();
    assert_eq!(lock.data.activity, Activity::Idle);

    // Update activity
    lock.update_activity(Activity::Working).unwrap();
    assert_eq!(lock.data.activity, Activity::Working);

    // Inspect presence from outside
    let presence = one_session::inspect_session_presence(&session_file);
    assert_eq!(
        presence,
        SessionPresence::Resident {
            activity: Activity::Working
        }
    );

    // Release lock
    lock.release();
    let presence_after = one_session::inspect_session_presence(&session_file);
    assert_eq!(presence_after, SessionPresence::Dormant);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_session_actor_persistence() {
    let tmp = unique_temp_dir();
    let session_file = tmp.join("actor_session.jsonl");

    let handle = SessionActor::spawn(session_file.clone(), "sess-actor-1".into(), 16);

    let entry1 = one_session::SessionEntry::SessionInfo {
        base: one_session::new_entry_base(None),
        name: "Test Actor Session".into(),
    };

    handle.append_entry(entry1).await.unwrap();
    handle.flush().await.unwrap();

    let content = tokio::fs::read_to_string(&session_file).await.unwrap();
    assert!(content.contains("Test Actor Session"));

    handle.shutdown().await;

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_global_discovery_trait() {
    let discovery = GlobalSessionDiscovery::new();
    let res = discovery.find_sessions("non_existent_random_pattern_xyz_123");
    assert!(res.is_ok());
    assert_eq!(res.unwrap().len(), 0);
}
