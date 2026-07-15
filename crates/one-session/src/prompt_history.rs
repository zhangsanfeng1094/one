//! Project-scoped prompt input history (Claude Code-style ↑/↓ recall).
//!
//! Stored under the same project session dir as JSONL sessions:
//! `~/.one/agent/sessions/--{cwd}--/prompt_history.jsonl`
//!
//! One JSON string per line. Newest prompts are last (readline order).
//! Survives process exit and `/new` so a fresh session still recalls prior work.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use one_core::message::AgentMessage;

use crate::manager::SessionManager;
use crate::paths::session_dir_for_cwd;
use crate::entries::SessionEntry;

const HISTORY_FILE: &str = "prompt_history.jsonl";
const MAX_ENTRIES: usize = 500;
/// How many recent session files to scan when seeding an empty history file.
const SEED_SESSION_LIMIT: usize = 20;

pub fn prompt_history_path(cwd: impl AsRef<Path>) -> PathBuf {
    session_dir_for_cwd(cwd.as_ref()).join(HISTORY_FILE)
}

/// Load persisted history (oldest → newest). Missing file → empty.
pub fn load_prompt_history(cwd: impl AsRef<Path>) -> Vec<String> {
    let path = prompt_history_path(cwd.as_ref());
    load_from_path(&path)
}

fn load_from_path(path: &Path) -> Vec<String> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Prefer JSON string; fall back to raw line for hand-edited files.
        let text = serde_json::from_str::<String>(line)
            .unwrap_or_else(|_| line.to_string());
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if out.last().map(|s: &String| s.as_str()) == Some(text) {
            continue;
        }
        out.push(text.to_string());
    }
    if out.len() > MAX_ENTRIES {
        out.drain(0..out.len() - MAX_ENTRIES);
    }
    out
}

/// Append one prompt (dedupe consecutive). Creates parent dirs as needed.
pub fn append_prompt_history(cwd: impl AsRef<Path>, text: &str) -> std::io::Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    let path = prompt_history_path(cwd.as_ref());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Skip if last line is the same (cheap consecutive dedupe).
    if let Some(last) = load_from_path(&path).last() {
        if last == text {
            return Ok(());
        }
    }

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let line = serde_json::to_string(text).unwrap_or_else(|_| text.to_string());
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Load history, or if empty seed from recent session user messages and persist.
///
/// This is what makes a **new process / new session** able to ↑ through prompts
/// from earlier conversations in the same project cwd.
pub async fn load_or_seed_prompt_history(cwd: impl AsRef<Path>) -> Vec<String> {
    let cwd = cwd.as_ref();
    let mut history = load_prompt_history(cwd);
    if !history.is_empty() {
        return history;
    }

    history = seed_from_sessions(cwd).await;
    if history.is_empty() {
        return history;
    }

    // Persist so next launch is instant and doesn't re-scan.
    if let Err(err) = rewrite_prompt_history(cwd, &history) {
        eprintln!("one: failed to write prompt history: {err}");
    }
    history
}

fn rewrite_prompt_history(cwd: &Path, entries: &[String]) -> std::io::Result<()> {
    let path = prompt_history_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    for text in entries {
        let line = serde_json::to_string(text).unwrap_or_else(|_| text.clone());
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// Collect user prompts from recent sessions (oldest → newest overall).
async fn seed_from_sessions(cwd: &Path) -> Vec<String> {
    let Ok(mut sessions) = SessionManager::list(cwd).await else {
        return Vec::new();
    };
    // Oldest first so ↑ walks from most recent at the end.
    sessions.sort_by_key(|s| s.modified);

    let mut out: Vec<String> = Vec::new();
    // Prefer most recent sessions; still walk chronological within each.
    let start = sessions.len().saturating_sub(SEED_SESSION_LIMIT);
    for info in &sessions[start..] {
        let Ok(manager) = SessionManager::open(&info.path).await else {
            continue;
        };
        // Prefer active-branch order for the leaf context; also scan all message
        // entries so forked branches contribute prompts.
        for entry in manager.entries() {
            if let SessionEntry::Message {
                message: AgentMessage::User(user),
                ..
            } = entry
            {
                let text = user.content.as_display_text();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if out.last().map(|s| s.as_str()) == Some(text) {
                    continue;
                }
                out.push(text.to_string());
            }
        }
    }

    if out.len() > MAX_ENTRIES {
        out.drain(0..out.len() - MAX_ENTRIES);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_history_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "one-prompt-history-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn append_and_load_roundtrip() {
        let path = temp_history_path("roundtrip");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", serde_json::to_string("hello").unwrap()).unwrap();
        writeln!(file, "{}", serde_json::to_string("world").unwrap()).unwrap();
        drop(file);

        let loaded = load_from_path(&path);
        assert_eq!(loaded, vec!["hello".to_string(), "world".to_string()]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn consecutive_dedupe_on_load() {
        let path = temp_history_path("dedupe");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "\"a\"").unwrap();
        writeln!(file, "\"a\"").unwrap();
        writeln!(file, "\"b\"").unwrap();
        drop(file);
        assert_eq!(load_from_path(&path), vec!["a".to_string(), "b".to_string()]);
        let _ = fs::remove_file(&path);
    }
}
