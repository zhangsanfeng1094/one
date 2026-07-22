use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub content: String,
    pub source: PathBuf,
}

pub async fn discover_prompts(dir: &Path) -> Result<Vec<PromptTemplate>> {
    let mut prompts = Vec::new();
    if !dir.exists() {
        return Ok(prompts);
    }

    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("prompt")
            .to_string();
        let content = fs::read_to_string(&path).await?;
        let description = content
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(&name)
            .to_string();
        prompts.push(PromptTemplate {
            name,
            description,
            content,
            source: path,
        });
    }
    Ok(prompts)
}

pub fn expand_prompt<'a>(prompts: &'a [PromptTemplate], input: &str) -> Option<&'a str> {
    if let Some(name) = input.strip_prefix('/') {
        prompts
            .iter()
            .find(|prompt| prompt.name == name)
            .map(|prompt| prompt.content.as_str())
    } else {
        None
    }
}
