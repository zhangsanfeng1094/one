use std::path::{Path, PathBuf};

use crate::agents::{load_agents_files, merge_agents_content, AgentsFile};
use crate::builtin_skills::load_builtin_skills;
use crate::error::Result;
use crate::prompts::{discover_prompts, PromptTemplate};
use crate::skills::{
    discover_skills, force_load_message, resolve_skill_invocation, skills_catalog_xml, Skill,
};

#[derive(Debug, Clone)]
pub struct ResourceLoader {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub agents_files: Vec<AgentsFile>,
    pub skills: Vec<Skill>,
    pub prompts: Vec<PromptTemplate>,
    pub system_append: Option<String>,
}

/// Result of expanding user input (prompt template and/or explicit skill force-load).
#[derive(Debug, Clone)]
pub struct ResolvedInput {
    pub text: String,
    /// Set when the user force-loaded a skill via `/skill:name`.
    pub skill: Option<String>,
}

impl ResourceLoader {
    pub async fn discover(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let agent_dir = agent_dir.as_ref().to_path_buf();

        let agents_files = load_agents_files(&cwd, &agent_dir).await?;

        // Precedence: project > user > builtin. discover_skills is first-found wins,
        // so pass project roots first. Builtins are merged only for names not yet seen.
        let mut skill_dirs = project_skill_dirs(&cwd);
        skill_dirs.extend(user_skill_dirs(&agent_dir));
        let mut skills = discover_skills(&skill_dirs).await?;
        merge_builtin_skills(&mut skills, &agent_dir).await?;

        let prompt_dirs = [agent_dir.join("prompts"), cwd.join(".one/prompts")];
        let mut prompts = Vec::new();
        for dir in prompt_dirs {
            prompts.extend(discover_prompts(&dir).await?);
        }

        Ok(Self {
            cwd,
            agent_dir,
            agents_files,
            skills,
            prompts,
            system_append: None,
        })
    }

    pub fn with_system_append(mut self, text: impl Into<String>) -> Self {
        self.system_append = Some(text.into());
        self
    }

    pub fn build_system_prompt(&self, base: &str) -> String {
        let mut parts = vec![base.to_string()];
        if !self.agents_files.is_empty() {
            parts.push(merge_agents_content(&self.agents_files));
        }
        // Tier 1 only: name + description + location (not full SKILL.md bodies).
        if let Some(catalog) = skills_catalog_xml(&self.skills) {
            parts.push(catalog);
        }
        if let Some(extra) = &self.system_append {
            parts.push(extra.clone());
        }
        parts.join("\n\n")
    }

    /// Expand prompt templates (`/name`) and **user-explicit** skill force-load (`/skill:name`).
    ///
    /// Normal chat does **not** inject skill bodies here — the model uses `read` on
    /// `<location>` when a catalog skill matches (progressive disclosure).
    pub fn resolve_input(&self, input: &str) -> ResolvedInput {
        if let Some((skill, extra)) = resolve_skill_invocation(&self.skills, input) {
            return ResolvedInput {
                text: force_load_message(skill, &extra),
                skill: Some(skill.name.clone()),
            };
        }

        let text = crate::prompts::expand_prompt(&self.prompts, input)
            .unwrap_or(input)
            .to_string();
        ResolvedInput { text, skill: None }
    }

    pub fn resolve_prompt(&self, input: &str) -> String {
        self.resolve_input(input).text
    }

    pub fn skill_names(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.name.clone()).collect()
    }

    /// Skills visible to the model (not disable-model-invocation).
    pub fn model_visible_skills(&self) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .collect()
    }
}

/// Project-scoped skill roots (higher precedence). Walks cwd → parents for `.agents/skills`.
fn project_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.join(".one/skills"), cwd.join(".agents/skills")];
    // Ancestor `.agents/skills` (monorepo / nested workdirs), up to filesystem root.
    let mut current = cwd.to_path_buf();
    while current.pop() {
        let candidate = current.join(".agents/skills");
        if candidate.is_dir() {
            dirs.push(candidate);
        }
        // Stop at git root if present (still allow above if no git — keep walking a bit).
        if current.join(".git").exists() {
            break;
        }
    }
    dirs
}

/// User-scoped skill roots.
fn user_skill_dirs(agent_dir: &Path) -> Vec<PathBuf> {
    let home = dirs_home();
    let mut dirs = vec![
        agent_dir.join("skills"), // ~/.one/agent/skills
        home.join(".agents/skills"),
    ];
    // Pragmatic compatibility with other harnesses (optional, low precedence).
    dirs.push(home.join(".claude/skills"));
    dirs.push(home.join(".codex/skills"));
    dirs.push(home.join(".grok/skills"));
    dirs
}

/// Append embedded skills (e.g. create-skill) when not already provided on disk.
async fn merge_builtin_skills(skills: &mut Vec<Skill>, agent_dir: &Path) -> Result<()> {
    let builtins = load_builtin_skills(agent_dir).await?;
    let mut seen: std::collections::HashSet<String> =
        skills.iter().map(|s| s.name.to_ascii_lowercase()).collect();
    for skill in builtins {
        if seen.insert(skill.name.to_ascii_lowercase()) {
            skills.push(skill);
        }
    }
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::parse_skill_md;
    use std::path::Path;

    #[tokio::test]
    async fn merge_skips_when_user_skill_exists() {
        let mut skills = vec![parse_skill_md(
            "---\nname: create-skill\ndescription: User override.\n---\n# Custom\n",
            Path::new("/tmp/user/create-skill/SKILL.md"),
        )
        .unwrap()];
        let tmp = std::env::temp_dir().join(format!(
            "one-merge-builtin-{}",
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        merge_builtin_skills(&mut skills, &tmp).await.unwrap();
        let creates: Vec<_> = skills.iter().filter(|s| s.name == "create-skill").collect();
        assert_eq!(creates.len(), 1);
        assert!(creates[0].body.contains("Custom"));
        assert!(!creates[0].body.contains("Create Skill"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn merge_adds_create_skill_when_missing() {
        let mut skills = Vec::new();
        let tmp = std::env::temp_dir().join(format!(
            "one-merge-builtin-add-{}",
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        merge_builtin_skills(&mut skills, &tmp).await.unwrap();
        assert!(skills.iter().any(|s| s.name == "create-skill"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
