//! Skills shipped inside the One binary.
//!
//! Materialized under `{agent_dir}/builtin-skills/<name>/SKILL.md` so the model
//! can `read` them (progressive disclosure). User/project skills with the same
//! name always win — callers should only inject builtins for names not already
//! discovered on disk.

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::Result;
use crate::skills::{parse_skill_md, Skill};

/// Embedded skill packages: `(directory_name, SKILL.md contents)`.
const BUILTIN_SKILL_MDS: &[(&str, &str)] = &[(
    "create-skill",
    include_str!("../skills/create-skill/SKILL.md"),
)];

/// Root directory for materialized builtin skills.
pub fn builtin_skills_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("builtin-skills")
}

/// Ensure every embedded skill is written to disk (refresh if content drifted)
/// and return parsed [`Skill`]s. Does not filter by name — caller merges with
/// discovered skills using first-found-wins.
pub async fn load_builtin_skills(agent_dir: &Path) -> Result<Vec<Skill>> {
    let root = builtin_skills_root(agent_dir);
    let mut out = Vec::with_capacity(BUILTIN_SKILL_MDS.len());

    for (dir_name, content) in BUILTIN_SKILL_MDS {
        let skill_dir = root.join(dir_name);
        let skill_path = skill_dir.join("SKILL.md");

        materialize_skill(&skill_dir, &skill_path, content).await?;

        if let Some(skill) = parse_skill_md(content, &skill_path) {
            out.push(skill);
        }
    }

    Ok(out)
}

async fn materialize_skill(skill_dir: &Path, skill_path: &Path, content: &str) -> Result<()> {
    let needs_write = match fs::read_to_string(skill_path).await {
        Ok(existing) => existing != content,
        Err(_) => true,
    };
    if !needs_write {
        return Ok(());
    }
    fs::create_dir_all(skill_dir).await?;
    fs::write(skill_path, content).await?;
    Ok(())
}

/// Names of all embedded builtins (for tests / diagnostics).
pub fn builtin_skill_names() -> Vec<&'static str> {
    BUILTIN_SKILL_MDS.iter().map(|(n, _)| *n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_skill_parses() {
        let (_, content) = BUILTIN_SKILL_MDS[0];
        let skill = parse_skill_md(content, Path::new("/virtual/create-skill/SKILL.md")).unwrap();
        assert_eq!(skill.name, "create-skill");
        assert!(skill.description.to_lowercase().contains("skill"));
        assert!(!skill.body.is_empty());
    }

    #[tokio::test]
    async fn materialize_and_load() {
        let tmp =
            std::env::temp_dir().join(format!("one-builtin-skills-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp).await;
        fs::create_dir_all(&tmp).await.unwrap();

        let skills = load_builtin_skills(&tmp).await.unwrap();
        assert!(skills.iter().any(|s| s.name == "create-skill"));

        let path = tmp.join("builtin-skills/create-skill/SKILL.md");
        assert!(path.is_file());
        let on_disk = fs::read_to_string(&path).await.unwrap();
        assert_eq!(on_disk, BUILTIN_SKILL_MDS[0].1);

        // Second load is a no-op rewrite
        let skills2 = load_builtin_skills(&tmp).await.unwrap();
        assert_eq!(skills2.len(), skills.len());

        let _ = fs::remove_dir_all(&tmp).await;
    }
}
