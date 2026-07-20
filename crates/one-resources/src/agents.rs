use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct AgentsFile {
    pub path: PathBuf,
    pub content: String,
}

/// Load AGENTS.md / CLAUDE.md from agent home + cwd walking **up to the git root**
/// (or filesystem root if no `.git` is found).
///
/// Stopping at the git root avoids swallowing unrelated parent monorepo docs when
/// the workspace is a nested checkout without its own git metadata... actually we
/// stop when we hit a directory containing `.git`, after including that directory's
/// files. Parents above the git root are not scanned.
pub async fn load_agents_files(cwd: &Path, agent_dir: &Path) -> Result<Vec<AgentsFile>> {
    let mut files = Vec::new();

    let global = agent_dir.join("AGENTS.md");
    if global.exists() {
        files.push(AgentsFile {
            path: global.clone(),
            content: fs::read_to_string(&global).await?,
        });
    }

    let mut current = cwd.to_path_buf();
    loop {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let candidate = current.join(name);
            if candidate.exists() && !files.iter().any(|file| file.path == candidate) {
                files.push(AgentsFile {
                    path: candidate.clone(),
                    content: fs::read_to_string(&candidate).await?,
                });
            }
        }
        // Stop after including the git worktree / repo root.
        if current.join(".git").exists() {
            break;
        }
        if !current.pop() {
            break;
        }
    }

    Ok(files)
}

pub fn merge_agents_content(files: &[AgentsFile]) -> String {
    files
        .iter()
        .map(|file| format!("# From {}\n\n{}", file.path.display(), file.content))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn stops_at_git_root() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root_path = std::env::temp_dir().join(format!("one-agents-test-{stamp}"));
        let _ = fs::remove_dir_all(&root_path);
        fs::create_dir_all(root_path.join(".git")).unwrap();
        fs::write(root_path.join("AGENTS.md"), "root agents").unwrap();

        let nested = root_path.join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("AGENTS.md"), "nested agents").unwrap();

        let agent_home = root_path.join("agent-home");
        fs::create_dir_all(&agent_home).unwrap();

        let files = load_agents_files(&nested, &agent_home).await.unwrap();
        let texts: Vec<_> = files.iter().map(|f| f.content.as_str()).collect();
        assert!(texts.contains(&"nested agents"));
        assert!(texts.contains(&"root agents"));

        let _ = fs::remove_dir_all(&root_path);
    }
}
