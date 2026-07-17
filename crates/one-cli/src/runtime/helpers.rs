//! Small shared helpers for the runtime module tree.

use std::path::PathBuf;

use one_core::compaction::is_context_overflow_error;
use one_core::error::OneError;
use one_ext::ExtensionRuntime;
use one_session::{agent_dir, SessionManager};
use uuid::Uuid;

pub(super) fn is_overflow_err(err: &OneError) -> bool {
    match err {
        OneError::ContextOverflow(_) => true,
        OneError::Provider(msg) => is_context_overflow_error(msg),
        _ => false,
    }
}

pub(super) fn load_extension_state(extensions: &ExtensionRuntime, session: &SessionManager) {
    for entry in session.entries() {
        if let one_session::SessionEntry::Custom {
            custom_type, data, ..
        } = entry
        {
            extensions.restore_custom(custom_type, data.clone());
        }
    }
}

pub(super) fn new_plan_path() -> PathBuf {
    agent_dir()
        .join("plans")
        .join(format!("{}.md", Uuid::new_v4()))
}
