use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct AgentsFile {
    pub path: PathBuf,
    pub content: String,
}

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