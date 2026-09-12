use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const AGENT_DIR_NAME: &str = ".one/agent";
pub const SESSIONS_DIR: &str = "sessions";

fn agent_dir_override() -> &'static Mutex<Option<PathBuf>> {
    static CELL: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Override agent dir root (tests / custom layouts).
pub fn set_agent_dir_override(path: Option<PathBuf>) -> Option<PathBuf> {
    let mut g = agent_dir_override()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *g, path)
}

pub fn agent_dir() -> PathBuf {
    if let Ok(g) = agent_dir_override().lock() {
        if let Some(ref p) = *g {
            return p.clone();
        }
    }
    if let Ok(dir) = std::env::var("ONE_AGENT_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir.trim());
        }
    }
    if let Ok(dir) = std::env::var("ONE_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir.trim()).join("agent");
        }
    }
    dirs_home().join(AGENT_DIR_NAME)
}

pub fn session_root() -> PathBuf {
    agent_dir().join(SESSIONS_DIR)
}

pub fn session_dir_for_cwd(cwd: &Path) -> PathBuf {
    let encoded = cwd
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "-");
    session_root().join(format!("--{encoded}--"))
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub const TERMINAL_DIR: &str = "terminal";

/// Return `<session_parent_dir>/terminal/` for saving terminal command output logs, matching Grok session structure.
pub fn terminal_dir_for_session(session_file: &Path) -> PathBuf {
    let parent = session_file.parent().unwrap_or(session_file);
    parent.join(TERMINAL_DIR)
}

/// Return `<session_parent_dir>/terminal/<task_id>.log`.
pub fn terminal_log_path(session_file: &Path, task_id: &str) -> PathBuf {
    terminal_dir_for_session(session_file).join(format!("{task_id}.log"))
}
