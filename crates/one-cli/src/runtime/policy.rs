//! PathPolicy construction from CLI + settings + skill roots.

use std::path::PathBuf;

use one_resources::{skill_allowlist_roots, ResourceLoader};
use one_tools::{PathPolicy, SandboxMode};

use crate::cli::Cli;

/// Build path policy from CLI + settings + discovered skills.
///
/// Priority: `--full-access` / CLI `--add-dir` override settings; settings fill gaps.
/// Skill discovery roots and package dirs are always readable (not writable) so
/// the model can load `SKILL.md` / bundled resources without `--add-dir`.
pub(super) fn build_path_policy(
    cwd: &std::path::Path,
    cli: &Cli,
    settings: &crate::settings::Settings,
    resources: &ResourceLoader,
) -> PathPolicy {
    let mode = if cli.full_access {
        SandboxMode::FullAccess
    } else if let Some(s) = settings.sandbox.as_deref().and_then(SandboxMode::parse) {
        s
    } else {
        SandboxMode::WorkspaceWrite
    };

    let mut policy = PathPolicy::workspace(cwd.to_path_buf()).with_mode(mode);

    let mut extras: Vec<PathBuf> = cli.add_dir.clone();
    if let Some(dirs) = &settings.additional_directories {
        for d in dirs {
            extras.push(PathBuf::from(d));
        }
    }
    // Dedup while preserving order.
    let mut seen = std::collections::HashSet::new();
    extras.retain(|p| seen.insert(p.clone()));
    if !extras.is_empty() {
        policy = policy.with_additional_dirs(extras);
    }

    // Progressive disclosure allowlist (agentskills.io / Codex).
    let skill_roots =
        skill_allowlist_roots(cwd, &resources.agent_dir, resources.all_skills());
    policy = policy.with_readable_roots(skill_roots);

    policy
}
