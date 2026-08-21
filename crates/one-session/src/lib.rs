pub mod actor;
pub mod context;
pub mod discovery;
pub mod entries;
pub mod error;
pub mod export;
pub mod manager;
pub mod meta;
pub mod migrate;
pub mod paths;
pub mod presence;
pub mod prompt_history;
pub mod sidecars;
pub mod summary;
#[cfg(feature = "network")]
pub mod share;

pub use actor::{PersistenceMsg, SessionActor, SessionActorHandle};
pub use context::{
    build_context_entries, build_session_context, context_message_entries, first_kept_entry_id,
    SessionContext,
};
pub use discovery::{GlobalSessionDiscovery, IndexableSession, SessionSource};
pub use entries::*;
pub use error::{Result, SessionError};
pub use export::export_html;
pub use manager::{RewindPointInfo, SessionInfo, SessionManager};
pub use meta::{
    prompt_hash, ErrorMeta, PromptSnapshotMeta, ToolAuditItem, ToolAuditMeta, UsageFields,
    UsageMeta, CUSTOM_ERROR, CUSTOM_PROMPT_SNAPSHOT, CUSTOM_TOOL_AUDIT, CUSTOM_USAGE,
    PROMPT_INLINE_MAX_BYTES,
};
pub use migrate::migrate_jsonl;
pub use paths::{agent_dir, session_dir_for_cwd, session_root};
pub use presence::{
    inspect_session_presence, is_process_alive, lock_path_for, Activity, SessionLock,
    SessionPresence,
};
pub use prompt_history::{
    append_prompt_history, load_or_seed_prompt_history, load_prompt_history, prompt_history_path,
};
pub use sidecars::{
    read_sidecar_json, read_sidecar_json_async, sidecar_path_for, write_sidecar_json,
    write_sidecar_json_async, FileHunkRecord, HunkSnapshotsSidecar, PlanSidecar, PromptHunkSnapshot,
    SidecarKind, TodoItemRecord, TodoSidecar,
};
pub use summary::{
    load_summary, summary_path_for, system_prompt_path_for, write_summary_file, SessionSummary,
    SUMMARY_SCHEMA,
};
#[cfg(feature = "network")]
pub use share::share_to_gist;
