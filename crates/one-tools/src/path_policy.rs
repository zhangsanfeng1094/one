//! Workspace path boundary for file tools.
//!
//! Default mode (`WorkspaceWrite`) only allows paths under the working directory
//! (plus `--add-dir` roots). Always-readable roots cover Agent Skills progressive
//! disclosure ([agentskills.io](https://agentskills.io)): agent home, cross-client
//! `~/.agents/skills`, and compat harness skill dirs (`~/.codex/skills`, etc.).
//! Use `FullAccess` / `--full-access` to disable the boundary (container / trusted
//! environments only).
//!
//! Interactive sessions may accumulate **read-only** dynamic grants (Once path /
//! Session root) behind a shared [`Arc`] so clones used by tools and the permission
//! gate see the same allowlist.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// Grants accumulated during an interactive session (read-only).
///
/// All paths stored here MUST already be policy-normalized (see `grant_*`).
#[derive(Debug, Default)]
pub struct DynamicGrants {
    /// Session-scoped always-readable roots (from "Session root" approval).
    readable_roots: Vec<PathBuf>,
    /// Paths allowed for Read: files (exact) or directories (dir + descendants).
    /// NOT the same as static `allowed_files` (exact + write-capable).
    allowed_paths: Vec<PathBuf>,
}

/// Snapshot of dynamic grants for subagent spawn (paths only; no Arc share).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportedReadGrants {
    /// From parent Session-root grants (and any `grant_readable_root`).
    pub readable_roots: Vec<PathBuf>,
    /// From parent Once grants (`grant_read_path` — files or dirs).
    pub allowed_paths: Vec<PathBuf>,
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
    /// Shared across clones from one AppRuntime / ToolBuildContext.
    dynamic: Arc<Mutex<DynamicGrants>>,
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
            dynamic: Arc::new(Mutex::new(DynamicGrants::default())),
        }
    }

    /// Unrestricted path policy (cwd still used for relative resolution).
    pub fn full_access(cwd: impl Into<PathBuf>) -> Self {
        let mut p = Self::workspace(cwd);
        p.mode = SandboxMode::FullAccess;
        p
    }

    /// Shared handle for `Arc::ptr_eq` checks (runtime ↔ gate ↔ tools).
    pub fn dynamic_handle(&self) -> Arc<Mutex<DynamicGrants>> {
        Arc::clone(&self.dynamic)
    }

    /// Replace the dynamic grant set (e.g. reattach after rebuilding static roots).
    pub fn with_shared_dynamic(mut self, dynamic: Arc<Mutex<DynamicGrants>>) -> Self {
        self.dynamic = dynamic;
        self
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
    ///
    /// **Write-capable** for plan-file exception. Do **not** use for interactive
    /// path-read Once grants — use [`Self::grant_read_path`] instead.
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

    /// Read grant: file = exact path; directory = path + descendants. Idempotent.
    ///
    /// Write is never granted by this API (dynamic grants are ignored for Write).
    pub fn grant_read_path(&self, path: impl AsRef<Path>) {
        let normalized = normalize_for_check(path.as_ref());
        let mut g = self.dynamic.lock().expect("dynamic grants lock");
        if !g.allowed_paths.iter().any(|p| paths_match(p, &normalized)) {
            g.allowed_paths.push(normalized);
        }
    }

    /// Read-only root for the session. Idempotent. Normalizes as existing dir.
    pub fn grant_readable_root(&self, root: impl AsRef<Path>) {
        let p = root.as_ref();
        let normalized = if p.is_dir() {
            normalize_existing_dir(p.to_path_buf())
        } else if let Some(parent) = p.parent() {
            // If caller passed a file, grant its parent directory.
            normalize_existing_dir(parent.to_path_buf())
        } else {
            normalize_existing_dir(p.to_path_buf())
        };
        let mut g = self.dynamic.lock().expect("dynamic grants lock");
        if !g.readable_roots.iter().any(|r| paths_match(r, &normalized)) {
            g.readable_roots.push(normalized);
        }
    }

    /// Snapshot dynamic grants for subagent spawn (copy of paths only; no Arc share).
    pub fn export_read_grants(&self) -> ExportedReadGrants {
        let g = self.dynamic.lock().expect("dynamic grants lock");
        ExportedReadGrants {
            readable_roots: g.readable_roots.clone(),
            allowed_paths: g.allowed_paths.clone(),
        }
    }

    /// Apply an export onto **this** policy's dynamic grants (typically a fresh
    /// child policy with a **new** `dynamic` Arc). Calls `grant_readable_root` /
    /// `grant_read_path` only. **Must not** touch `allowed_files` / `with_allowed_file`.
    pub fn apply_exported_read_grants(&self, exported: &ExportedReadGrants) {
        for r in &exported.readable_roots {
            self.grant_readable_root(r);
        }
        for p in &exported.allowed_paths {
            self.grant_read_path(p);
        }
    }

    /// Suggest a session-scoped read-only root for an outside path, or `None`
    /// when only Once (exact path) should be offered (sensitive trees, `/`, `$HOME`).
    ///
    /// `resolved` should already be absolute / policy-normalized.
    pub fn suggest_read_root(&self, resolved: &Path) -> Option<PathBuf> {
        suggest_read_root_impl(resolved)
    }

    /// Shared error text for outside-workspace (tools + gate write deny).
    pub fn format_outside_error(&self, path: &Path, access: AccessKind) -> String {
        let kind = match access {
            AccessKind::Read => "read",
            AccessKind::Write => "write",
        };
        let roots: Vec<&Path> = match access {
            AccessKind::Read => self.readable_roots().collect(),
            AccessKind::Write => self.writable_roots().collect(),
        };
        let mut msg = format!(
            "path outside workspace ({kind} denied): {}\n\
             Allowed roots: {}\n\
             Use --add-dir <path> to grant access, or --full-access to disable the boundary.",
            path.display(),
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if matches!(access, AccessKind::Read) {
            msg.push_str(
                "\nPath boundary is independent of always-approve / --yes. \
                 Re-run without Always-approve, pass --add-dir at launch, or approve a path \
                 grant in a normal interactive session.",
            );
        }
        msg
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

    /// Readable roots: writable + always-readable (static only; dynamic checked separately).
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
        // Opaque git objects/index are never useful as text to the model and burn
        // explore turns. Always refuse (even full-access) so agents use `git` via bash.
        if matches!(access, AccessKind::Read) && is_opaque_git_path(path) {
            return Err(format!(
                "refusing to read opaque git path `{}` — use bash \
                 (`git status --short`, `git diff --stat`, `git diff --cached --stat`) \
                 instead of reading `.git/index` / `.git/objects`.",
                path.display()
            ));
        }

        if self.mode == SandboxMode::FullAccess {
            return Ok(());
        }

        let normalized = normalize_for_check(path);

        // Exact allowed files (plan file, etc.) — write-capable by design.
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

        // Dynamic read grants (Once path / Session root) — Read only.
        if matches!(access, AccessKind::Read) && self.dynamic_read_allows(&normalized, &lexical) {
            return Ok(());
        }

        Err(self.format_outside_error(path, access))
    }

    fn dynamic_read_allows(&self, normalized: &Path, lexical: &Path) -> bool {
        let g = self.dynamic.lock().expect("dynamic grants lock");
        for p in &g.allowed_paths {
            if paths_match(p, normalized)
                || paths_match(p, lexical)
                || is_within(p, normalized)
                || is_within(p, lexical)
            {
                return true;
            }
        }
        for root in &g.readable_roots {
            if is_within(root, normalized) || is_within(root, lexical) {
                return true;
            }
        }
        false
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
        home.join(".one").join("docs"),
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

/// True for git metadata that is binary or useless as model text input.
///
/// Allowed (text): `.git/HEAD`, `refs/**`, `logs/**`, `COMMIT_EDITMSG`, `config`.
/// Denied: `.git/index`, `.git/objects/**` (explore was burning turns on these).
pub fn is_opaque_git_path(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect();
    let Some(git_i) = parts.iter().position(|p| p.as_ref() == ".git") else {
        return false;
    };
    let rest = &parts[git_i + 1..];
    if rest.is_empty() {
        return false;
    }
    match rest[0].as_ref() {
        "objects" | "index" | "index.lock" => true,
        _ => false,
    }
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

/// Suggest a session read root for an outside path, or None for Once-only.
fn suggest_read_root_impl(resolved: &Path) -> Option<PathBuf> {
    let home = dirs_home();
    let home_norm = normalize_existing_dir(home.clone());

    // Find nearest existing directory ancestor (or the path itself if a dir).
    let mut candidate = if resolved.is_dir() {
        normalize_existing_dir(resolved.to_path_buf())
    } else {
        let mut cur = resolved.to_path_buf();
        loop {
            match cur.parent() {
                Some(parent) if parent != cur.as_path() => {
                    cur = parent.to_path_buf();
                    if cur.is_dir() || cur.exists() {
                        break normalize_existing_dir(cur);
                    }
                }
                _ => return None,
            }
        }
    };

    // If the file doesn't exist, still walk lexical parents for an existing dir.
    if !candidate.exists() {
        let mut cur = clean_path(resolved);
        loop {
            if cur.is_dir() || (cur.exists() && cur.is_dir()) {
                candidate = normalize_existing_dir(cur);
                break;
            }
            match cur.parent() {
                Some(parent) if parent != cur.as_path() => cur = parent.to_path_buf(),
                _ => return None,
            }
        }
    }

    // Demote: filesystem root
    if candidate.parent().is_none()
        || candidate == Path::new("/")
        || candidate.as_os_str() == std::ffi::OsStr::new("/")
    {
        return None;
    }

    // Demote: $HOME itself
    if paths_match(&candidate, &home_norm) || paths_match(&candidate, &home) {
        return None;
    }

    // Demote: path under sensitive home subtrees → Once only
    if under_sensitive_home(resolved, &home_norm) || under_sensitive_home(&candidate, &home_norm) {
        return None;
    }

    Some(candidate)
}

/// Well-known secret / credential trees under $HOME — no Session root offer.
fn under_sensitive_home(path: &Path, home_norm: &Path) -> bool {
    let path = clean_path(path);
    let home = clean_path(home_norm);
    if !path.starts_with(&home) {
        return false;
    }
    let Ok(rel) = path.strip_prefix(&home) else {
        return false;
    };
    let mut comps = rel.components();
    let Some(Component::Normal(first)) = comps.next() else {
        return false;
    };
    let first = first.to_string_lossy();
    // Single-segment sensitive dirs
    const SENSITIVE_TOP: &[&str] = &[
        ".ssh", ".gnupg", ".aws", ".kube", ".docker", ".netrc", ".npmrc", ".mozilla",
    ];
    if SENSITIVE_TOP.iter().any(|s| first == *s) {
        return true;
    }
    // Nested under .config
    if first == ".config" {
        if let Some(Component::Normal(second)) = comps.next() {
            let second = second.to_string_lossy();
            const SENSITIVE_CONFIG: &[&str] = &["gcloud", "google-chrome", "chromium", "gh"];
            if SENSITIVE_CONFIG.iter().any(|s| second == *s) {
                return true;
            }
        }
    }
    false
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
    fn opaque_git_paths_detected() {
        assert!(is_opaque_git_path(Path::new("/proj/.git/index")));
        assert!(is_opaque_git_path(Path::new("/proj/.git/index.lock")));
        assert!(is_opaque_git_path(Path::new(
            "/proj/.git/objects/ab/cdef1234"
        )));
        assert!(!is_opaque_git_path(Path::new("/proj/.git/HEAD")));
        assert!(!is_opaque_git_path(Path::new("/proj/.git/refs/heads/main")));
        assert!(!is_opaque_git_path(Path::new("/proj/.git/COMMIT_EDITMSG")));
        assert!(!is_opaque_git_path(Path::new("/proj/src/main.rs")));
    }

    #[test]
    fn refuse_read_git_index_and_objects() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        let index = dir.join(".git").join("index");
        let obj = dir.join(".git").join("objects").join("aa").join("bb");
        let head = dir.join(".git").join("HEAD");
        std::fs::create_dir_all(obj.parent().unwrap()).unwrap();
        std::fs::write(&index, b"\0bin").unwrap();
        std::fs::write(&obj, b"x").unwrap();
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();

        let err = policy
            .check(&index, AccessKind::Read)
            .expect_err("index must be denied");
        assert!(err.contains("opaque git"), "{err}");
        let err = policy
            .check(&obj, AccessKind::Read)
            .expect_err("objects must be denied");
        assert!(err.contains("opaque git"), "{err}");
        // Text git metadata remains readable when under workspace.
        policy
            .check(&head, AccessKind::Read)
            .expect("HEAD is text metadata");

        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn clone_shares_dynamic_grants() {
        let dir = temp_dir();
        let outside = temp_dir();
        let file = outside.join("secret.txt");
        std::fs::write(&file, "x").unwrap();

        let a = PathPolicy::workspace(dir.clone());
        let b = a.clone();
        assert!(
            Arc::ptr_eq(&a.dynamic_handle(), &b.dynamic_handle()),
            "clone must share dynamic Arc"
        );

        a.check(&file, AccessKind::Read).expect_err("before grant");
        a.grant_read_path(&file);
        b.check(&file, AccessKind::Read)
            .expect("grant on A visible on B");
        a.check(&file, AccessKind::Write)
            .expect_err("dynamic grants are read-only");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_dir_allows_descendants_read_not_write() {
        let dir = temp_dir();
        let outside = temp_dir();
        let nested = outside.join("sub").join("a.rs");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "fn main() {}").unwrap();

        let policy = PathPolicy::workspace(dir.clone());
        policy.grant_read_path(&outside);
        policy
            .check(&nested, AccessKind::Read)
            .expect("Once-on-dir covers descendants");
        policy
            .check(&nested, AccessKind::Write)
            .expect_err("write still denied");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builders_preserve_dynamic_arc() {
        let dir = temp_dir();
        let plan = dir.join("plan.md");
        std::fs::write(&plan, "# p").unwrap();
        let base = PathPolicy::workspace(dir.clone());
        let handle = base.dynamic_handle();
        let with_file = base.clone().with_allowed_file(plan);
        let with_mode = base.clone().with_mode(SandboxMode::WorkspaceWrite);
        let with_extra = base.clone().with_additional_dirs([temp_dir()]);
        assert!(Arc::ptr_eq(&handle, &with_file.dynamic_handle()));
        assert!(Arc::ptr_eq(&handle, &with_mode.dynamic_handle()));
        assert!(Arc::ptr_eq(&handle, &with_extra.dynamic_handle()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggest_read_root_sensitive_ssh_none() {
        let home = dirs_home();
        let ssh = home.join(".ssh").join("id_rsa");
        assert!(
            suggest_read_root_impl(&ssh).is_none(),
            "sensitive ~/.ssh must be Once-only"
        );
        let aws = home.join(".aws").join("credentials");
        assert!(suggest_read_root_impl(&aws).is_none());
    }

    #[test]
    fn suggest_read_root_codex_config() {
        let home = dirs_home();
        let codex = home.join(".codex");
        // Only assert when parent exists so CI without ~/.codex still passes the algorithm.
        if codex.is_dir() {
            let cfg = codex.join("config.toml");
            let suggested = suggest_read_root_impl(&cfg);
            assert!(
                suggested
                    .as_ref()
                    .map(|p| paths_match(p, &normalize_existing_dir(codex.clone())))
                    .unwrap_or(false),
                "expected ~/.codex, got {suggested:?}"
            );
        }
    }

    #[test]
    fn export_apply_detached_read_only() {
        let dir = temp_dir();
        let outside = temp_dir();
        let file = outside.join("a.txt");
        std::fs::write(&file, "hi").unwrap();

        let parent = PathPolicy::workspace(dir.clone());
        parent.grant_read_path(&file);
        parent.grant_readable_root(&outside);
        let exported = parent.export_read_grants();

        let child = PathPolicy::workspace(dir.clone());
        assert!(!Arc::ptr_eq(
            &parent.dynamic_handle(),
            &child.dynamic_handle()
        ));
        child.apply_exported_read_grants(&exported);
        child
            .check(&file, AccessKind::Read)
            .expect("child sees exported Once path");
        child
            .check(&file, AccessKind::Write)
            .expect_err("export must not enable Write");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_shared_dynamic_reattach() {
        let dir = temp_dir();
        let a = PathPolicy::workspace(dir.clone());
        let handle = a.dynamic_handle();
        let b = PathPolicy::workspace(dir.clone()).with_shared_dynamic(handle.clone());
        assert!(Arc::ptr_eq(&handle, &b.dynamic_handle()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
