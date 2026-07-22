//! Workspace path boundary for file tools.
//!
//! Default mode (`WorkspaceWrite`) only allows paths under the working directory
//! (plus `--add-dir` roots). Always-readable roots cover Agent Skills progressive
//! disclosure ([agentskills.io](https://agentskills.io)): agent home, cross-client
//! `~/.agents/skills`, and compat harness skill dirs (`~/.codex/skills`, etc.).
//! Use `FullAccess` / `--full-access` to disable the boundary (container / trusted
//! environments only).

use std::path::{Component, Path, PathBuf};

/// How a tool intends to use a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
}

/// Filesystem sandbox posture for path tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Paths must fall under workspace roots (cwd + add-dir).
    /// Skill discovery roots + agent home are readable (plans / SKILL.md).
    #[default]
    WorkspaceWrite,
    /// No path boundary (dangerous on a host machine).
    FullAccess,
}

impl SandboxMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "workspace" | "workspace-write" | "workspace_write" | "default" => {
                Some(Self::WorkspaceWrite)
            }
            "full" | "full-access" | "full_access" | "danger" | "danger-full-access" => {
                Some(Self::FullAccess)
            }
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "full-access",
        }
    }
}

/// Policy applied by read/write/edit/grep/find/ls (and plan read tools).
#[derive(Debug, Clone)]
pub struct PathPolicy {
    /// Canonical (or cleaned) working directory.
    cwd: PathBuf,
    /// Extra roots the agent may read and write.
    additional_roots: Vec<PathBuf>,
    /// Always-readable roots (skills, plans under agent home).
    readable_roots: Vec<PathBuf>,
    /// Specific files allowed for read+write outside roots (e.g. plan file).
    allowed_files: Vec<PathBuf>,
    mode: SandboxMode,
}

impl PathPolicy {
    /// Workspace-scoped policy for `cwd`. Canonicalizes when possible.
    pub fn workspace(cwd: impl Into<PathBuf>) -> Self {
        let cwd = normalize_existing_dir(cwd.into());
        let mut readable_roots = Vec::new();
        // agentskills.io permission allowlist: skill roots are read-only by default
        // so the model can `read` catalog `location` paths (and bundled resources).
        for root in default_skill_readable_roots() {
            let p = if root.exists() {
                normalize_existing_dir(root)
            } else {
                clean_path(&root)
            };
            if !readable_roots.iter().any(|r| r == &p) {
                readable_roots.push(p);
            }
        }
        Self {
            cwd,
            additional_roots: Vec::new(),
            readable_roots,
            allowed_files: Vec::new(),
            mode: SandboxMode::WorkspaceWrite,
        }
    }

    /// Unrestricted path policy (cwd still used for relative resolution).
    pub fn full_access(cwd: impl Into<PathBuf>) -> Self {
        let mut p = Self::workspace(cwd);
        p.mode = SandboxMode::FullAccess;
        p
    }

    pub fn with_mode(mut self, mode: SandboxMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_additional_dirs<I, P>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        for d in dirs {
            let p = normalize_existing_dir(d.into());
            if !self.additional_roots.iter().any(|r| r == &p) {
                self.additional_roots.push(p);
            }
        }
        self
    }

    /// Allow a single file outside roots (e.g. plan markdown under `~/.one/agent/plans`).
    pub fn with_allowed_file(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        // Prefer canonical if the file already exists.
        let p = std::fs::canonicalize(&p).unwrap_or_else(|_| clean_path(&p));
        if !self.allowed_files.iter().any(|f| f == &p) {
            self.allowed_files.push(p);
        }
        self
    }

    /// Extra always-readable root (e.g. custom skill location).
    pub fn with_readable_root(mut self, path: impl Into<PathBuf>) -> Self {
        let p = normalize_existing_dir(path.into());
        if !self.readable_roots.iter().any(|r| r == &p) {
            self.readable_roots.push(p);
        }
        self
    }

    /// Batch-add always-readable roots (skill discovery dirs / package dirs).
    pub fn with_readable_roots<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        for path in paths {
            self = self.with_readable_root(path);
        }
        self
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn is_full_access(&self) -> bool {
        self.mode == SandboxMode::FullAccess
    }

    /// Writable roots: cwd + additional directories.
    pub fn writable_roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.cwd.as_path()).chain(self.additional_roots.iter().map(|p| p.as_path()))
    }

    /// Readable roots: writable + always-readable.
    pub fn readable_roots(&self) -> impl Iterator<Item = &Path> {
        self.writable_roots()
            .chain(self.readable_roots.iter().map(|p| p.as_path()))
    }

    /// Resolve a tool path against cwd and enforce the policy.
    ///
    /// Returns an absolute path suitable for filesystem ops.
    pub fn resolve(&self, path: &str, access: AccessKind) -> Result<PathBuf, String> {
        if path.is_empty() {
            return Err("path is empty".into());
        }
        let resolved = resolve_against_cwd(&self.cwd, path);
        self.check(&resolved, access)?;
        Ok(resolved)
    }

    /// Check an already-joined path (absolute or relative-to-cwd).
    pub fn check(&self, path: &Path, access: AccessKind) -> Result<(), String> {
        if self.mode == SandboxMode::FullAccess {
            return Ok(());
        }

        let normalized = normalize_for_check(path);

        // Exact allowed files (plan file, etc.).
        if self
            .allowed_files
            .iter()
            .any(|f| paths_match(f, &normalized) || paths_match(f, path))
        {
            return Ok(());
        }

        let roots: Vec<&Path> = match access {
            AccessKind::Read => self.readable_roots().collect(),
            AccessKind::Write => self.writable_roots().collect(),
        };

        if roots.iter().any(|root| is_within(root, &normalized)) {
            return Ok(());
        }

        // Also try matching non-canonical input against roots (symlink edge cases).
        let lexical = clean_path(path);
        if roots.iter().any(|root| is_within(root, &lexical)) {
            return Ok(());
        }

        let kind = match access {
            AccessKind::Read => "read",
            AccessKind::Write => "write",
        };
        Err(format!(
            "path outside workspace ({kind} denied): {}\n\
             Allowed roots: {}\n\
             Use --add-dir <path> to grant access, or --full-access to disable the boundary.",
            path.display(),
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn default_agent_dir() -> PathBuf {
    // Mirror one_session::agent_dir without taking a dependency on one-session.
    let home = dirs_home();
    home.join(".one").join("agent")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Default read-only skill roots (Codex / agentskills convention).
///
/// Keep in sync with `one_resources::skill_discovery_dirs` user roots.
/// Runtime also merges discovered package dirs via [`PathPolicy::with_readable_roots`].
fn default_skill_readable_roots() -> Vec<PathBuf> {
    let home = dirs_home();
    let agent = default_agent_dir();
    vec![
        agent.clone(),
        agent.join("skills"),
        agent.join("builtin-skills"),
        // Cross-client shared install location (agentskills.io).
        home.join(".agents").join("skills"),
        // Client-native / compat harnesses (lower discovery precedence, still readable).
        home.join(".claude").join("skills"),
        home.join(".codex").join("skills"),
        home.join(".grok").join("skills"),
    ]
}

fn normalize_existing_dir(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or_else(|_| clean_path(&path))
}

/// Resolve relative paths against cwd; leave absolute paths as-is, then normalize.
pub fn resolve_against_cwd(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    normalize_for_check(&joined)
}

/// Prefer real path via canonicalize of longest existing prefix.
fn normalize_for_check(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }

    // Walk up to an existing ancestor, then re-append the missing tail.
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut out = canon;
            for part in missing.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match cur.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                match cur.parent() {
                    Some(parent) if parent != cur.as_path() => cur = parent.to_path_buf(),
                    _ => break,
                }
            }
            None => break,
        }
    }

    clean_path(path)
}

/// Lexical cleanup: drop `.` and resolve `..` without touching the filesystem.
fn clean_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn is_within(root: &Path, path: &Path) -> bool {
    let root = clean_path(root);
    let path = clean_path(path);
    if path == root {
        return true;
    }
    path.starts_with(&root)
}

fn paths_match(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    clean_path(a) == clean_path(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "one-path-policy-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn allows_relative_inside_workspace() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        let resolved = policy.resolve("src/main.rs", AccessKind::Write).unwrap();
        assert!(resolved.starts_with(&dir) || resolved.starts_with(policy.cwd()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denies_absolute_outside_workspace() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        let err = policy.resolve("/etc/passwd", AccessKind::Read).unwrap_err();
        assert!(err.contains("outside workspace"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denies_parent_escape() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        // ../ from inside workspace should land outside.
        let err = policy
            .resolve("../escape.txt", AccessKind::Write)
            .unwrap_err();
        assert!(err.contains("outside workspace"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_dir_grants_write() {
        let workspace = temp_dir();
        let extra = temp_dir();
        let policy = PathPolicy::workspace(workspace.clone()).with_additional_dirs([extra.clone()]);
        let target = extra.join("note.txt");
        let resolved = policy
            .resolve(target.to_str().unwrap(), AccessKind::Write)
            .unwrap();
        assert!(resolved.ends_with("note.txt"));
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&extra);
    }

    #[test]
    fn full_access_allows_absolute() {
        let dir = temp_dir();
        let policy = PathPolicy::full_access(dir.clone());
        let resolved = policy.resolve("/etc/passwd", AccessKind::Read).unwrap();
        assert_eq!(resolved, PathBuf::from("/etc/passwd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowed_file_outside_workspace() {
        let dir = temp_dir();
        let plan = std::env::temp_dir().join(format!("one-plan-allow-{}.md", std::process::id()));
        std::fs::write(&plan, "# plan").unwrap();
        let policy = PathPolicy::workspace(dir.clone()).with_allowed_file(plan.clone());
        let resolved = policy
            .resolve(plan.to_str().unwrap(), AccessKind::Write)
            .unwrap();
        assert!(paths_match(&resolved, &plan) || resolved.ends_with(plan.file_name().unwrap()));
        let _ = std::fs::remove_file(&plan);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_mode_parse() {
        assert_eq!(
            SandboxMode::parse("workspace-write"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            SandboxMode::parse("full-access"),
            Some(SandboxMode::FullAccess)
        );
        assert!(SandboxMode::parse("nope").is_none());
    }

    #[test]
    fn skill_roots_readable_not_writable() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());

        // Default policy includes ~/.agents/skills as a readable root (agentskills.io).
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let agents_skill = home
                .join(".agents")
                .join("skills")
                .join("x")
                .join("SKILL.md");
            policy
                .check(&agents_skill, AccessKind::Read)
                .expect("default skill root should be readable");
            let write_err = policy
                .check(&agents_skill, AccessKind::Write)
                .expect_err("skill root must stay read-only");
            assert!(write_err.contains("outside workspace"), "{write_err}");

            let codex_skill = home
                .join(".codex")
                .join("skills")
                .join("git-weekly-summary")
                .join("SKILL.md");
            policy
                .check(&codex_skill, AccessKind::Read)
                .expect("compat ~/.codex/skills should be readable");
        }

        let extra = temp_dir();
        let skill_md = extra.join("my-skill").join("SKILL.md");
        std::fs::create_dir_all(skill_md.parent().unwrap()).unwrap();
        std::fs::write(&skill_md, "---\nname: t\ndescription: d\n---\n").unwrap();
        let policy = PathPolicy::workspace(dir.clone()).with_readable_root(extra.clone());
        policy
            .resolve(skill_md.to_str().unwrap(), AccessKind::Read)
            .expect("allowlisted skill package is readable");
        let write_err = policy
            .resolve(skill_md.to_str().unwrap(), AccessKind::Write)
            .expect_err("readable skill root is not writable");
        assert!(write_err.contains("outside workspace"), "{write_err}");

        let _ = std::fs::remove_dir_all(&extra);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
