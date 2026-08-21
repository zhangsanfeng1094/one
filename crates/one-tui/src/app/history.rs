//! Prompt history (↑/↓ recall) with optional project-scoped persistence.

use std::path::PathBuf;

/// Path helper kept local so one-tui does not hard-depend on one-session types
/// beyond the path layout we already share via the CLI wiring.
pub(crate) fn one_session_prompt_history_path(cwd: &std::path::Path) -> PathBuf {
    // Mirror one_session::paths::session_dir_for_cwd + prompt_history.jsonl
    // so tests / App can show the path without importing session crate in all builds.
    // Actual I/O goes through `persist_append_prompt_history` (CLI-linked).
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let encoded = cwd
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "-");
    home.join(".one/agent/sessions")
        .join(format!("--{encoded}--"))
        .join("prompt_history.jsonl")
}

pub(crate) fn persist_append_prompt_history(
    cwd: &std::path::Path,
    text: &str,
) -> std::io::Result<()> {
    // Inline minimal append so one-tui stays free of one-session if needed.
    // Format matches one_session::prompt_history (JSON string per line).
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    let path = one_session_prompt_history_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(text).unwrap_or_else(|_| text.to_string());
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

impl super::App {
    /// Replace in-memory history (e.g. load from disk / past sessions).
    /// Does **not** write back — caller already owns the file contents.
    pub fn load_prompt_history(&mut self, entries: Vec<String>) {
        self.prompt_history = entries;
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Enable project-scoped persistence (Claude: history survives new sessions).
    ///
    /// `cwd` is the project directory used for `~/.one/agent/sessions/--cwd--/prompt_history.jsonl`.
    pub fn enable_prompt_history_persist(&mut self, cwd: impl Into<PathBuf>) {
        let cwd = cwd.into();
        self.history_persist_path = Some(one_session_prompt_history_path(&cwd));
        self.history_cwd = Some(cwd);
    }

    /// Record a prompt into ↑/↓ history (dedupes consecutive identical entries).
    /// When persistence is enabled, also appends to the project history file.
    pub fn push_prompt_history(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref().trim();
        if text.is_empty() {
            return;
        }
        if self.prompt_history.last().map(|s| s.as_str()) == Some(text) {
            return;
        }
        self.prompt_history.push(text.to_string());
        // Cap growth so long sessions stay snappy.
        const MAX: usize = 500;
        if self.prompt_history.len() > MAX {
            let drop_n = self.prompt_history.len() - MAX;
            self.prompt_history.drain(0..drop_n);
        }
        self.history_index = None;
        self.history_draft.clear();

        if let Some(cwd) = &self.history_cwd {
            // Best-effort; history recall must not fail the UI.
            let _ = persist_append_prompt_history(cwd, text);
        }
    }

    pub fn prompt_history_len(&self) -> usize {
        self.prompt_history.len()
    }

    pub fn history_prev(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_draft = self.input.clone();
                let i = self.prompt_history.len() - 1;
                self.history_index = Some(i);
                self.input = self.prompt_history[i].clone();
                self.input_cursor_end();
            }
            Some(0) => {
                // Already at oldest — stay put.
            }
            Some(i) => {
                let i = i - 1;
                self.history_index = Some(i);
                self.input = self.prompt_history[i].clone();
                self.input_cursor_end();
            }
        }
        self.pending_images.clear();
        self.pending_texts.clear();
        self.cursor_on = true;
        self.clear_notice();
    }

    /// Step newer (Down / Ctrl+N). Restores the draft past the newest entry.
    pub fn history_next(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.prompt_history.len() {
            let i = i + 1;
            self.history_index = Some(i);
            self.input = self.prompt_history[i].clone();
        } else {
            self.history_index = None;
            self.input = std::mem::take(&mut self.history_draft);
        }
        self.input_cursor_end();
        self.pending_images.clear();
        self.pending_texts.clear();
        self.cursor_on = true;
        self.clear_notice();
    }

    /// Leave history browse mode without changing the buffer (after typing, etc.).
    pub(crate) fn leave_history_browse(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }
}
