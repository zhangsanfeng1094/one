use std::path::PathBuf;

/// `~/.one/agent` — same layout as one-session.
pub fn agent_dir() -> PathBuf {
    dirs_home().join(".one").join("agent")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
