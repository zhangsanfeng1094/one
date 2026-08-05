pub mod context;
pub mod entries;
pub mod error;
pub mod export;
pub mod manager;
pub mod meta;
pub mod migrate;
pub mod paths;
pub mod prompt_history;
pub mod summary;
#[cfg(feature = "network")]
pub mod share;

pub use context::{
    build_context_entries, build_session_context, context_message_entries, first_kept_entry_id,
    SessionContext,
};
pub use entries::*;
pub use error::{Result, SessionError};
pub use export::export_html;
pub use manager::{SessionInfo, SessionManager};
pub use meta::{
    prompt_hash, ErrorMeta, PromptSnapshotMeta, ToolAuditItem, ToolAuditMeta, UsageFields,
    UsageMeta, CUSTOM_ERROR, CUSTOM_PROMPT_SNAPSHOT, CUSTOM_TOOL_AUDIT, CUSTOM_USAGE,
    PROMPT_INLINE_MAX_BYTES,
};
pub use migrate::migrate_jsonl;
pub use paths::{agent_dir, session_dir_for_cwd};
pub use prompt_history::{
    append_prompt_history, load_or_seed_prompt_history, load_prompt_history, prompt_history_path,
};
pub use summary::{
    load_summary, summary_path_for, system_prompt_path_for, write_summary_file, SessionSummary,
    SUMMARY_SCHEMA,
};
#[cfg(feature = "network")]
pub use share::share_to_gist;
