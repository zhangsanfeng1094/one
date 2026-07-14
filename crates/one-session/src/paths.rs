use std::path::{Path, PathBuf};

pub const AGENT_DIR_NAME: &str = ".one/agent";
pub const SESSIONS_DIR: &str = "sessions";

pub fn agent_dir() -> PathBuf {
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