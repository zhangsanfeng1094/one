//! Git worktree isolation for harness runs (no auto-merge).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::protocol::{error_code, ProtocolError, WorktreeInfo};

const WT_DIR: &str = "worktrees";

#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub repo_root: PathBuf,
}

impl WorktreeHandle {
    pub fn to_info(&self, kept: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: self.path.display().to_string(),
            branch: self.branch.clone(),
            base_ref: self.base_ref.clone(),
            kept,
        }
    }
}

/// Create / remove git worktrees under `<repo>/.one/worktrees/<id>`.
pub struct WorktreeManager;

impl WorktreeManager {
    /// Create a new worktree branched from `HEAD` of `repo` (or its git root).
    pub fn create(repo: &Path, job_id: &str) -> Result<WorktreeHandle, ProtocolError> {
        let repo_root = find_git_root(repo).ok_or_else(|| {
            ProtocolError::new(
                error_code::INVALID_REQUEST,
                format!(
                    "isolation=worktree requires a git repository (no .git above {})",
                    repo.display()
                ),
            )
        })?;
        let id = sanitize_id(job_id);
        let branch = format!("one/task-{id}");
        let path = repo_root.join(".one").join(WT_DIR).join(&id);

        if path.exists() {
            // Stale path from a previous crash — try force-remove first.
            let _ = Self::remove_at(&repo_root, &path, &branch, true);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ProtocolError::new(
                    error_code::INTERNAL,
                    format!("create worktree parent {}: {e}", parent.display()),
                )
            })?;
        }

        let base_ref = git_stdout(&repo_root, &["rev-parse", "--short", "HEAD"])?;
        let base_ref = base_ref.trim().to_string();

        // Prefer named branch; if branch exists, check out detached at HEAD into path.
        let add = git_status(
            &repo_root,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &path.display().to_string(),
                "HEAD",
            ],
        );
        if let Err(e) = add {
            // Branch may already exist — try without -b.
            let retry = git_status(
                &repo_root,
                &["worktree", "add", &path.display().to_string(), &branch],
            );
            if retry.is_err() {
                return Err(ProtocolError::new(
                    error_code::INTERNAL,
                    format!("git worktree add failed: {e}"),
                ));
            }
        }

        Ok(WorktreeHandle {
            path,
            branch,
            base_ref,
            repo_root,
        })
    }

    /// Remove worktree directory and prune. Optionally delete the branch.
    pub fn remove(handle: &WorktreeHandle, delete_branch: bool) -> Result<(), ProtocolError> {
        Self::remove_at(
            &handle.repo_root,
            &handle.path,
            &handle.branch,
            delete_branch,
        )
    }

    fn remove_at(
        repo_root: &Path,
        path: &Path,
        branch: &str,
        delete_branch: bool,
    ) -> Result<(), ProtocolError> {
        let path_s = path.display().to_string();
        // force unlock even if dirty
        let _ = git_status(repo_root, &["worktree", "remove", "--force", &path_s]);
        if path.exists() {
            let _ = std::fs::remove_dir_all(path);
        }
        let _ = git_status(repo_root, &["worktree", "prune"]);
        if delete_branch {
            let _ = git_status(repo_root, &["branch", "-D", branch]);
        }
        Ok(())
    }

    /// After a run: keep on failure (default), drop on success.
    pub fn cleanup_after_run(handle: &WorktreeHandle, run_ok: bool) -> bool {
        if run_ok {
            let _ = Self::remove(handle, true);
            false // not kept
        } else {
            true // kept for inspection
        }
    }
}

fn sanitize_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if s.is_empty() {
        format!("t{}", std::process::id())
    } else {
        s
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn git_status(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, ProtocolError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| ProtocolError::new(error_code::INTERNAL, format!("spawn git: {e}")))?;
    if !out.status.success() {
        return Err(ProtocolError::new(
            error_code::INTERNAL,
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_init_fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-wt-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.join("README"), "hi").unwrap();
        assert!(Command::new("git")
            .args(["add", "README"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        dir
    }

    #[test]
    fn worktree_create_and_remove() {
        let dir = git_init_fixture();
        let h = WorktreeManager::create(&dir, "job_test1").expect("create");
        assert!(h.path.is_dir());
        assert!(h.path.join("README").is_file());
        // Write only in worktree
        std::fs::write(h.path.join("only-wt.txt"), "x").unwrap();
        assert!(!dir.join("only-wt.txt").exists());
        WorktreeManager::remove(&h, true).unwrap();
        assert!(!h.path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn not_a_git_repo_errors() {
        let dir = std::env::temp_dir().join(format!("one-nongit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let err = WorktreeManager::create(&dir, "x").unwrap_err();
        assert!(err.message.contains("git"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
