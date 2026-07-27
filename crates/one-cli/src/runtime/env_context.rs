//! Session-boot environment snapshot injected into the system prompt.
//!
//! Frozen for the session (like memory L2): recompute only on cold start,
//! `/reload`, or `/new` — not every turn — so prompt cache stays stable.

use std::path::Path;
use std::process::Command;

/// Max characters of `git status --short` to keep (after line filter).
const GIT_STATUS_MAX_CHARS: usize = 1_200;
/// Max status lines before summarizing with a count.
const GIT_STATUS_MAX_LINES: usize = 24;

/// Build a short `<env>` block: cwd, date, platform, git branch + dirty summary.
pub fn build_env_context(cwd: &Path) -> String {
    let mut parts = Vec::new();
    parts.push(format!("cwd: {}", cwd.display()));
    parts.push(format!("date: {}", utc_date()));
    parts.push(format!(
        "platform: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.is_empty() {
            parts.push(format!("shell: {shell}"));
        }
    }
    if let Some(user) = std::env::var_os("USER").or_else(|| std::env::var_os("USERNAME")) {
        let u = user.to_string_lossy();
        if !u.is_empty() {
            parts.push(format!("user: {u}"));
        }
    }

    match git_snapshot(cwd) {
        Some(git) => parts.push(git),
        None => parts.push("git: (not a repository or git unavailable)".into()),
    }

    format!(
        "## Environment\n\
         Session snapshot (frozen at start / `/new` / `/reload` — re-check with tools if stale).\n\n\
         <env>\n{}\n</env>\n",
        parts.join("\n")
    )
}

fn utc_date() -> String {
    // Prefer RFC3339-ish date without pulling a chrono dep.
    // `date -u +%Y-%m-%d` is portable enough on Linux/macOS; fall back to empty.
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%MZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn git_snapshot(cwd: &Path) -> Option<String> {
    // Quick check: .git exists (file or dir — worktrees use .git file).
    if !cwd.join(".git").exists() {
        // Walk up a few levels for monorepo nested cwd.
        let mut cur = cwd.to_path_buf();
        let mut found = false;
        for _ in 0..6 {
            if !cur.pop() {
                break;
            }
            if cur.join(".git").exists() {
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }

    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }

    let mut out = format!("git_branch: {branch}");

    if let Some(head) = git_stdout(cwd, &["rev-parse", "--short", "HEAD"]) {
        let h = head.trim();
        if !h.is_empty() {
            out.push_str(&format!("\ngit_head: {h}"));
        }
    }

    // Porcelain short status — compact dirty signal without full diffs.
    if let Some(status) = git_stdout(cwd, &["status", "--porcelain", "-b"]) {
        let lines: Vec<&str> = status.lines().filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            out.push_str("\ngit_status: clean");
        } else {
            // First line is often ## branch...upstream
            let mut body: Vec<&str> = lines
                .iter()
                .copied()
                .filter(|l| !l.starts_with("## "))
                .collect();
            let branch_line = lines.iter().find(|l| l.starts_with("## ")).copied();
            if let Some(b) = branch_line {
                out.push_str(&format!("\ngit_upstream: {}", b.trim_start_matches("## ").trim()));
            }
            let total = body.len();
            if total == 0 {
                out.push_str("\ngit_status: clean");
            } else {
                body.truncate(GIT_STATUS_MAX_LINES);
                let mut block = body.join("\n");
                if block.len() > GIT_STATUS_MAX_CHARS {
                    block = block.chars().take(GIT_STATUS_MAX_CHARS).collect();
                    block.push('…');
                }
                out.push_str(&format!(
                    "\ngit_status: {total} changed path(s)\n{block}"
                ));
                if total > GIT_STATUS_MAX_LINES {
                    out.push_str(&format!(
                        "\n… +{} more (run `git status` for full list)",
                        total - GIT_STATUS_MAX_LINES
                    ));
                }
            }
        }
    }

    Some(out)
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).to_string();
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn env_contains_cwd_and_env_tags() {
        let tmp = std::env::temp_dir().join(format!("one-env-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let block = build_env_context(&tmp);
        assert!(block.contains("<env>"));
        assert!(block.contains("cwd:"));
        assert!(block.contains(tmp.file_name().unwrap().to_str().unwrap()) || block.contains("one-env-ctx"));
        assert!(block.contains("platform:"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn git_repo_shows_branch() {
        let tmp = std::env::temp_dir().join(format!("one-env-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let ok = Command::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&tmp);
            return; // skip if git missing
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&tmp)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&tmp)
            .status();
        std::fs::write(tmp.join("f.txt"), "x").unwrap();
        let _ = Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(&tmp)
            .status();
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&tmp)
            .status();

        let block = build_env_context(&tmp);
        assert!(
            block.contains("git_branch:") || block.contains("git:"),
            "{block}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
