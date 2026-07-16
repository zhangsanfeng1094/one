use std::path::{Path, PathBuf};

use crate::agents::{load_agents_files, merge_agents_content, AgentsFile};
use crate::builtin_skills::load_builtin_skills;
use crate::error::Result;
use crate::prompts::{discover_prompts, PromptTemplate};
use crate::skills::{
    apply_skills_config, discover_skills, force_load_message, resolve_skill_invocation,
    skills_catalog_xml, Skill, SkillConfigEntry,
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
        // Roots follow agentskills.io: client-specific + `.agents/skills` convention.
        let skill_dirs = skill_discovery_dirs(&cwd, &agent_dir);
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
        self.skills
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())
            .collect()
    }

    /// All discovered skills (including user-disabled) for management UI.
    pub fn all_skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Skills visible to the model (enabled and not disable-model-invocation).
    pub fn model_visible_skills(&self) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|s| s.enabled && !s.disable_model_invocation)
            .collect()
    }

    /// Apply Codex-style enable/disable config and keep catalog in sync.
    pub fn apply_skills_config(&mut self, config: &[SkillConfigEntry]) {
        apply_skills_config(&mut self.skills, config);
    }

    /// Find skill by name (any enable state; for management / status).
    pub fn find_skill(&self, name: &str) -> Option<&Skill> {
        self.skills
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// Find skill by path (for toggle by path).
    pub fn find_skill_by_path(&self, path: &Path) -> Option<&Skill> {
        self.skills.iter().find(|s| {
            s.location == path
                || s.location.to_string_lossy() == path.to_string_lossy()
                || (s.location.file_name() == path.file_name()
                    && s.location.parent().and_then(|p| p.file_name())
                        == path.parent().and_then(|p| p.file_name()))
        })
    }
}

/// All skill roots scanned at startup (project first, then user).
///
/// Mirrors [agentskills.io client integration](https://agentskills.io/client-implementation/adding-skills-support):
/// each scope scans the client-native dir and the cross-client `.agents/skills` convention.
pub fn skill_discovery_dirs(cwd: &Path, agent_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = project_skill_dirs(cwd);
    dirs.extend(user_skill_dirs(agent_dir));
    dirs
}

/// Roots that must be **readable** so progressive disclosure works (`read` SKILL.md +
/// bundled `scripts/` / `references/`).
///
/// Per the Agent Skills guide: *if the agent has a permission system that gates file
/// access, allowlist skill directories*. Includes discovery roots, agent home (builtin
/// skills / plans), and each skill package directory (covers symlink targets after
/// canonicalize).
pub fn skill_allowlist_roots(cwd: &Path, agent_dir: &Path, skills: &[Skill]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !roots.iter().any(|r| r == &p) {
            roots.push(p);
        }
    };

    push(agent_dir.to_path_buf());
    // Builtin skills land under agent_dir/builtin-skills.
    push(agent_dir.join("builtin-skills"));
    push(agent_dir.join("skills"));

    for d in skill_discovery_dirs(cwd, agent_dir) {
        push(d);
    }

    // Package dirs for discovered skills (and their real paths if symlinked).
    for skill in skills {
        let base = skill.base_dir().to_path_buf();
        push(base.clone());
        if let Ok(canon) = std::fs::canonicalize(&base) {
            push(canon);
        }
        // Parent skill root (e.g. ~/.codex/skills) when package is one level down.
        if let Some(parent) = skill.base_dir().parent() {
            push(parent.to_path_buf());
            if let Ok(canon) = std::fs::canonicalize(parent) {
                push(canon);
            }
        }
    }

    roots
}

/// Project-scoped skill roots (higher precedence).
///
/// Order: client-native `.one/skills`, then cross-client `.agents/skills`, then
/// ancestor `.agents/skills` (monorepo) up to git root.
fn project_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.join(".one/skills"), cwd.join(".agents/skills")];
    // Ancestor `.agents/skills` (monorepo / nested workdirs).
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
///
/// Order (agentskills.io + Codex-style):
/// 1. client-native `~/.one/agent/skills`
/// 2. cross-client `~/.agents/skills` (shared install location)
/// 3. pragmatic compat: `~/.claude` / `~/.codex` / `~/.grok` skills (lower precedence)
fn user_skill_dirs(agent_dir: &Path) -> Vec<PathBuf> {
    let home = dirs_home();
    vec![
        agent_dir.join("skills"),
        home.join(".agents/skills"),
        home.join(".claude/skills"),
        home.join(".codex/skills"),
        home.join(".grok/skills"),
    ]
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

    #[test]
    fn discovery_dirs_include_agents_convention() {
        let cwd = Path::new("/tmp/proj");
        let agent = Path::new("/tmp/agent-home");
        let dirs = skill_discovery_dirs(cwd, agent);
        assert!(dirs.contains(&cwd.join(".one/skills")));
        assert!(dirs.contains(&cwd.join(".agents/skills")));
        assert!(dirs.contains(&agent.join("skills")));
        let user_agents = dirs_home().join(".agents/skills");
        let user_codex = dirs_home().join(".codex/skills");
        let agents_idx = dirs.iter().position(|d| d == &user_agents);
        let codex_idx = dirs.iter().position(|d| d == &user_codex);
        assert!(agents_idx.is_some(), "user ~/.agents/skills required");
        assert!(codex_idx.is_some(), "compat ~/.codex/skills required");
        assert!(
            agents_idx.unwrap() < codex_idx.unwrap(),
            ".agents/skills should outrank .codex/skills"
        );
    }

    #[test]
    fn allowlist_includes_skill_package_and_discovery_roots() {
        let cwd = Path::new("/tmp/proj");
        let agent = Path::new("/tmp/agent-home");
        let skill = parse_skill_md(
            "---\nname: weekly\ndescription: d\n---\nbody\n",
            Path::new("/home/u/.codex/skills/git-weekly-summary/SKILL.md"),
        )
        .unwrap();
        let roots = skill_allowlist_roots(cwd, agent, std::slice::from_ref(&skill));
        assert!(roots.iter().any(|r| r == agent));
        assert!(roots.contains(&skill.base_dir().to_path_buf()));
        assert!(roots.contains(&PathBuf::from("/home/u/.codex/skills")));
        assert!(roots.contains(&dirs_home().join(".agents/skills")));
    }
}
