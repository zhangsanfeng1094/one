pub mod context;
pub mod entries;
pub mod error;
pub mod export;
pub mod manager;
pub mod migrate;
pub mod paths;
pub mod prompt_history;
#[cfg(feature = "network")]
pub mod share;

pub use context::{SessionContext, build_context_entries, build_session_context};
pub use entries::*;
pub use error::{Result, SessionError};
pub use export::export_html;
pub use manager::{SessionInfo, SessionManager};
pub use migrate::migrate_jsonl;
pub use paths::{agent_dir, session_dir_for_cwd};
pub use prompt_history::{
    append_prompt_history, load_or_seed_prompt_history, load_prompt_history, prompt_history_path,
};
#[cfg(feature = "network")]
pub use share::share_to_gist;