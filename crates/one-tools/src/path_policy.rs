//! Workspace path boundary for file tools.
//!
//! Default mode (`WorkspaceWrite`) only allows paths under the working directory
//! (plus `--add-dir` roots and ephemeral temp dirs `/tmp`, `/var/tmp`, `$TMPDIR`).
//! Always-readable roots cover Agent Skills progressive disclosure
//! ([agentskills.io](https://agentskills.io)): agent home, cross-client
//! `~/.agents/skills`, and compat harness skill dirs (`~/.codex/skills`, etc.).
//! Use `FullAccess` / `--full-access` to disable the boundary (container / trusted
//! environments only).
//!
//! Interactive sessions may accumulate dynamic grants (Once path / Session root)
//! behind a shared [`Arc`] so clones used by tools and the permission gate see
//! the same allowlist.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How a tool intends to use a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
}

/// Result of resolving a **search/list** path for `grep` / `ls`.
///
/// [`Self::Narrowed`] is used when the requested directory is outside the
/// boundary but one or more readable roots sit underneath it (e.g. `~/.one`
/// while `~/.one/agent` is readable). Callers must search/list **only** those
/// roots — never readdir the requested ancestor (sibling leaks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadCoverage {
    Exact(PathBuf),
    Narrowed {
        requested: PathBuf,
        roots: Vec<PathBuf>,
    },
}

impl ReadCoverage {
    pub fn roots(&self) -> &[PathBuf] {
        match self {
            Self::Exact(p) => std::slice::from_ref(p),
            Self::Narrowed { roots, .. } => roots,
        }
    }

    pub fn requested(&self) -> &Path {
        match self {
            Self::Exact(p) => p,
            Self::Narrowed { requested, .. } => requested,
        }
    }

    pub fn is_narrowed(&self) -> bool {
        matches!(self, Self::Narrowed { .. })
    }
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

/// Grants accumulated during an interactive session.
///
/// All paths stored here MUST already be policy-normalized (see `grant_*`).
#[derive(Debug, Default, Clone)]
pub struct DynamicGrants {
    /// Session-scoped always-readable roots (from "Session root" approval).
    readable_roots: Vec<PathBuf>,
    /// Paths allowed for Read: files (exact) or directories (dir + descendants).
    /// NOT the same as static `allowed_files` (exact + write-capable).
    allowed_paths: Vec<PathBuf>,
    /// Session-scoped writable roots (from path-write "Session root" approval).
    writable_roots: Vec<PathBuf>,
    /// Paths allowed for Write: files (exact) or directories (dir + descendants).
    writable_paths: Vec<PathBuf>,
    /// Opaque capabilities injected into one effective tool call. They are not
    /// exported or considered by ordinary policy checks.
    once: Vec<OnceGrant>,
}

#[derive(Debug, Clone)]
struct OnceGrant {
    token: String,
    path: PathBuf,
    access: AccessKind,
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
    workspace_root: PathBuf,
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
        let allowed_files = default_writable_agent_config_files();
        // /tmp · /var/tmp · $TMPDIR: ephemeral, already readable for uploads;
        // writable so probes/scripts don't have to live in the project tree.
        let additional_roots = default_temp_roots()
            .into_iter()
            .map(|p| {
                if p.exists() {
                    normalize_existing_dir(p)
                } else {
                    clean_path(&p)
                }
            })
            .collect();
        Self {
            workspace_root: cwd.clone(),
            cwd,
            additional_roots,
            readable_roots,
            allowed_files,
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

    /// Child agents retain the parent's static capability envelope, but receive
    /// a detached dynamic set containing only explicitly session-scoped reads.
    /// The child's cwd changes relative-path resolution; it never becomes a
    /// new readable or writable root.
    pub fn derive_for_child(&self, cwd: impl Into<PathBuf>) -> Self {
        let mut child = self.clone();
        child.cwd = normalize_existing_dir(cwd.into());
        child.dynamic = Arc::new(Mutex::new(DynamicGrants::default()));
        child.apply_exported_read_grants(&self.export_read_grants());
        child
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
            let p = normalize_existing_dir(expand_user_pathbuf(d.into()));
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
        let p = expand_user_pathbuf(path.into());
        // Prefer canonical if the file already exists.
        let p = std::fs::canonicalize(&p).unwrap_or_else(|_| clean_path(&p));
        if !self.allowed_files.iter().any(|f| f == &p) {
            self.allowed_files.push(p);
        }
        self
    }

    /// Extra always-readable root (e.g. custom skill location).
    pub fn with_readable_root(mut self, path: impl Into<PathBuf>) -> Self {
        let p = normalize_existing_dir(expand_user_pathbuf(path.into()));
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
    /// Does **not** grant Write — use [`Self::grant_write_path`].
    pub fn grant_read_path(&self, path: impl AsRef<Path>) {
        let normalized = normalize_for_check(&expand_user_pathbuf(path.as_ref().to_path_buf()));
        let mut g = self.dynamic.lock().expect("dynamic grants lock");
        if !g.allowed_paths.iter().any(|p| paths_match(p, &normalized)) {
            g.allowed_paths.push(normalized);
        }
    }

    /// Read-only root for the session. Idempotent. Normalizes as existing dir.
    pub fn grant_readable_root(&self, root: impl AsRef<Path>) {
        let expanded = expand_user_pathbuf(root.as_ref().to_path_buf());
        let p = expanded.as_path();
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

    /// Write grant: file = exact path; directory = path + descendants. Also
    /// grants Read on the same path. Idempotent.
    pub fn grant_write_path(&self, path: impl AsRef<Path>) {
        let normalized = normalize_for_check(&expand_user_pathbuf(path.as_ref().to_path_buf()));
        self.grant_read_path(&normalized);
        let mut g = self.dynamic.lock().expect("dynamic grants lock");
        if !g.writable_paths.iter().any(|p| paths_match(p, &normalized)) {
            g.writable_paths.push(normalized);
        }
    }

    /// Grant a path only to the effective call carrying `token`.
    pub fn grant_once(&self, token: impl Into<String>, path: impl AsRef<Path>, access: AccessKind) {
        let path = normalize_for_check(&expand_user_pathbuf(path.as_ref().to_path_buf()));
        self.dynamic
            .lock()
            .expect("dynamic grants lock")
            .once
            .push(OnceGrant {
                token: token.into(),
                path,
                access,
            });
    }

    pub fn revoke_once(&self, token: &str) {
        self.dynamic
            .lock()
            .expect("dynamic grants lock")
            .once
            .retain(|g| g.token != token);
    }

    /// Session-scoped writable root (dir + descendants). Also grants Read.
    /// Idempotent. Normalizes as existing dir (file → parent).
    pub fn grant_writable_root(&self, root: impl AsRef<Path>) {
        let expanded = expand_user_pathbuf(root.as_ref().to_path_buf());
        let p = expanded.as_path();
        let normalized = if p.is_dir() {
            normalize_existing_dir(p.to_path_buf())
        } else if let Some(parent) = p.parent() {
            normalize_existing_dir(parent.to_path_buf())
        } else {
            normalize_existing_dir(p.to_path_buf())
        };
        self.grant_readable_root(&normalized);
        let mut g = self.dynamic.lock().expect("dynamic grants lock");
        if !g.writable_roots.iter().any(|r| paths_match(r, &normalized)) {
            g.writable_roots.push(normalized);
        }
    }

    /// Snapshot dynamic grants for subagent spawn (copy of paths only; no Arc share).
    pub fn export_read_grants(&self) -> ExportedReadGrants {
        let g = self.dynamic.lock().expect("dynamic grants lock");
        ExportedReadGrants {
            readable_roots: g.readable_roots.clone(),
            // Exact "Once" paths and opaque one-call grants do not cross an
            // agent boundary. Only an explicit session root is inheritable.
            allowed_paths: vec![],
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

    /// Session write root for an outside path, or `None` for Once-only.
    ///
    /// Same demotion as read (`/`, `$HOME`, sensitive trees), plus agent home
    /// (`~/.one/agent`) so one click cannot unlock `auth.json` / sessions.
    pub fn suggest_write_root(&self, resolved: &Path) -> Option<PathBuf> {
        let suggested = suggest_read_root_impl(resolved)?;
        let agent = normalize_existing_dir(default_agent_dir());
        if is_within(&agent, &suggested) || paths_match(&suggested, &agent) {
            return None;
        }
        Some(suggested)
    }

    /// Shared error text for outside-workspace (tools + gate write deny).
    pub fn format_outside_error(&self, path: &Path, access: AccessKind) -> String {
        let kind = match access {
            AccessKind::Read => "read",
            AccessKind::Write => "write",
        };
        let normalized = normalize_for_check(path);
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
            if let Some(parent) = self.cwd.parent() {
                let cwd = clean_path(&self.cwd);
                if is_within(parent, &normalized) && !is_within(&cwd, &normalized) {
                    msg.push_str(&format!(
                        "\nThis is a sibling of workspace `{}` — `~/` is only $HOME shorthand, \
                         not a workspace alias. Approve the path or pass --add-dir.",
                        cwd.display()
                    ));
                }
            }
        }
        if matches!(access, AccessKind::Write) {
            let agent = default_agent_dir();
            if is_within(&agent, &normalized) || is_within(&clean_path(&agent), &clean_path(path)) {
                msg.push_str(
                    "\nAgent home is read-only except config files `models.json`, \
                     `settings.json`, and `mcp.json`. Sessions, auth.json, and .env stay denied.",
                );
            }
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
        std::iter::once(self.workspace_root.as_path())
            .chain(self.additional_roots.iter().map(|p| p.as_path()))
    }

    /// Readable roots: writable + always-readable (static only; dynamic checked separately).
    pub fn readable_roots(&self) -> impl Iterator<Item = &Path> {
        self.writable_roots()
            .chain(self.readable_roots.iter().map(|p| p.as_path()))
    }

    /// Resolve a tool path against cwd and enforce the policy.
    ///
    /// Returns an absolute path suitable for filesystem ops.
    /// `~/` and `~` expand to `$HOME` (they are **not** workspace-relative).
    pub fn resolve(&self, path: &str, access: AccessKind) -> Result<PathBuf, String> {
        self.resolve_with_token(path, access, None)
    }

    /// Resolve with an opaque one-call capability supplied by the permission gate.
    pub fn resolve_with_token(
        &self,
        path: &str,
        access: AccessKind,
        token: Option<&str>,
    ) -> Result<PathBuf, String> {
        if path.is_empty() {
            return Err("path is empty".into());
        }
        let resolved = resolve_against_cwd(&self.cwd, path);
        self.check_with_token(&resolved, access, token)?;
        Ok(resolved)
    }

    /// Resolve a `grep` / `ls` path, narrowing to overlapping readable roots
    /// when the requested directory is an ancestor of allowlisted trees.
    pub fn resolve_read(&self, path: &str) -> Result<ReadCoverage, String> {
        self.resolve_read_with_token(path, None)
    }

    pub fn resolve_read_with_token(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<ReadCoverage, String> {
        if path.is_empty() {
            return Err("path is empty".into());
        }
        let resolved = resolve_against_cwd(&self.cwd, path);
        if self
            .check_with_token(&resolved, AccessKind::Read, token)
            .is_ok()
        {
            return Ok(ReadCoverage::Exact(resolved));
        }
        let roots = self.overlapping_readable_roots(&resolved);
        if roots.is_empty() {
            return Err(self.format_outside_error(&resolved, AccessKind::Read));
        }
        Ok(ReadCoverage::Narrowed {
            requested: resolved,
            roots,
        })
    }

    /// Check a path using a single effective-call capability if supplied.
    pub fn check_with_token(
        &self,
        path: &Path,
        access: AccessKind,
        token: Option<&str>,
    ) -> Result<(), String> {
        match self.check(path, access) {
            Ok(()) => Ok(()),
            Err(error) => {
                let normalized = normalize_for_check(path);
                let allowed = token.is_some_and(|token| {
                    self.dynamic
                        .lock()
                        .expect("dynamic grants lock")
                        .once
                        .iter()
                        .any(|g| {
                            g.token == token
                                && g.access == access
                                && paths_match(&g.path, &normalized)
                        })
                });
                if allowed {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Readable roots (static + dynamic) that sit **under** `ancestor`.
    ///
    /// Used to search `~/.one` without listing sibling secrets next to
    /// `~/.one/agent`. Nested roots are collapsed to the most general set.
    pub fn overlapping_readable_roots(&self, ancestor: &Path) -> Vec<PathBuf> {
        let ancestor_norm = normalize_for_check(ancestor);
        let ancestor_lex = clean_path(ancestor);
        let mut roots: Vec<PathBuf> = Vec::new();

        let mut consider = |root: &Path| {
            let r = normalize_for_check(root);
            if !(is_within(&ancestor_norm, &r) || is_within(&ancestor_lex, &r)) {
                return;
            }
            // Skip the ancestor itself — that case is Exact via check().
            if paths_match(&r, &ancestor_norm) || paths_match(&r, &ancestor_lex) {
                return;
            }
            push_minimal_root(&mut roots, r);
        };

        for root in self.readable_roots() {
            consider(root);
        }
        if let Ok(g) = self.dynamic.lock() {
            for p in &g.allowed_paths {
                consider(p);
            }
            for root in &g.readable_roots {
                consider(root);
            }
        }
        roots.sort();
        roots
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

        if matches!(access, AccessKind::Write) && self.dynamic_write_allows(&normalized, &lexical) {
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

    fn dynamic_write_allows(&self, normalized: &Path, lexical: &Path) -> bool {
        let g = self.dynamic.lock().expect("dynamic grants lock");
        for p in &g.writable_paths {
            if paths_match(p, normalized)
                || paths_match(p, lexical)
                || is_within(p, normalized)
                || is_within(p, lexical)
            {
                return true;
            }
        }
        for root in &g.writable_roots {
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
/// Temp dirs are **not** here — they are writable via [`default_temp_roots`].
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

/// Ephemeral temp roots: readable **and** writable without `--add-dir`.
///
/// Agents routinely drop probe scripts and uploads here (`/tmp/one_ws_probe.py`).
/// These directories are sticky-bit world-writable by design; they are not a
/// secret tree. `$HOME` and project dirs stay bounded.
fn default_temp_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let tmp = PathBuf::from("/tmp");
    if tmp.is_dir() || tmp.exists() {
        roots.push(tmp);
    }
    let td = std::env::temp_dir();
    if td.as_os_str() != "/tmp" && (td.is_dir() || td.parent().is_some()) {
        roots.push(td);
    }
    let var_tmp = PathBuf::from("/var/tmp");
    if var_tmp.is_dir() {
        roots.push(var_tmp);
    }
    roots
}

/// `models.json` / `settings.json` / `mcp.json` under agent home — write-capable
/// so the agent can apply user-approved provider config without `--add-dir`.
/// Sessions, `auth.json`, and `.env` stay outside this list.
fn default_writable_agent_config_files() -> Vec<PathBuf> {
    let agent = default_agent_dir();
    ["models.json", "settings.json", "mcp.json"]
        .into_iter()
        .map(|name| {
            let p = agent.join(name);
            std::fs::canonicalize(&p).unwrap_or_else(|_| clean_path(&p))
        })
        .collect()
}

fn expand_user_pathbuf(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.as_ref() == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        PathBuf::from(expand_tilde_str(&s))
    } else {
        path
    }
}

/// Expand `~` / `~/…` to `$HOME`. `~otheruser` is left unchanged.
pub fn expand_tilde_str(path: &str) -> String {
    if path == "~" {
        return dirs_home().to_string_lossy().into_owned();
    }
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));
    if let Some(rest) = rest {
        let mut home = dirs_home();
        if !rest.is_empty() {
            home.push(rest);
        }
        return home.to_string_lossy().into_owned();
    }
    path.to_string()
}

fn normalize_existing_dir(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or_else(|_| clean_path(&path))
}

/// Resolve relative paths against cwd; leave absolute paths as-is, then normalize.
///
/// `~/foo` is `$HOME/foo`, **not** `{cwd}/~/foo`.
pub fn resolve_against_cwd(cwd: &Path, path: &str) -> PathBuf {
    let expanded = expand_tilde_str(path);
    let p = Path::new(&expanded);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    normalize_for_check(&joined)
}

fn push_minimal_root(roots: &mut Vec<PathBuf>, candidate: PathBuf) {
    if roots.iter().any(|r| is_within(r, &candidate)) {
        return;
    }
    roots.retain(|r| !is_within(&candidate, r));
    roots.push(candidate);
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

    // Prefer the enclosing git work tree (sibling repo root) over a nested folder.
    if let Some(git_root) = enclosing_git_root(&candidate) {
        if git_root.parent().is_some()
            && !paths_match(&git_root, &home_norm)
            && !paths_match(&git_root, &home)
            && !under_sensitive_home(&git_root, &home_norm)
        {
            candidate = git_root;
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

fn enclosing_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        let git = cur.join(".git");
        if git.is_dir() || git.is_file() {
            return Some(normalize_existing_dir(cur));
        }
        match cur.parent() {
            Some(parent) if parent != cur.as_path() => cur = parent.to_path_buf(),
            _ => return None,
        }
    }
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

    fn isolated_outside_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target");
        let dir = base.join(format!(
            "test-isolated-{}-{}-{}",
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
        // Workspace must not sit under /tmp — `../` would land in the default
        // writable temp root and no longer count as an escape.
        let dir = isolated_outside_dir();
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

        let extra = isolated_outside_dir();
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
    fn tmp_roots_readable_and_writable_by_default() {
        let ws = isolated_outside_dir();
        let policy = PathPolicy::workspace(ws.clone());

        let tmp_file =
            std::env::temp_dir().join(format!("one-test-img-{}.png", std::process::id()));
        std::fs::write(&tmp_file, b"png").unwrap();
        policy
            .check(&tmp_file, AccessKind::Read)
            .expect("/tmp paths should be readable by default");
        policy
            .check(&tmp_file, AccessKind::Write)
            .expect("/tmp paths should be writable by default");
        policy
            .resolve("/tmp/one_ws_probe.py", AccessKind::Write)
            .expect("write /tmp/probe.py without --add-dir");

        let _ = std::fs::remove_file(&tmp_file);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn clone_shares_dynamic_grants() {
        let dir = temp_dir();
        let outside = isolated_outside_dir();
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
            .expect_err("read grant does not enable Write");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_dir_allows_descendants_read_not_write() {
        let dir = temp_dir();
        let outside = isolated_outside_dir();
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
    fn suggest_read_root_prefers_git_root() {
        let repo = isolated_outside_dir();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("internal").join("thinking").join("apply.go");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "package thinking\n").unwrap();

        let suggested = suggest_read_root_impl(&nested).expect("git root");
        assert!(
            paths_match(&suggested, &normalize_existing_dir(repo.clone())),
            "expected repo root {repo:?}, got {suggested:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sibling_project_error_mentions_tilde_is_home() {
        let ws = isolated_outside_dir();
        let sibling = isolated_outside_dir();
        let file = sibling.join("apply.go");
        std::fs::write(&file, "x").unwrap();
        let policy = PathPolicy::workspace(ws.clone());
        let err = policy
            .check(&file, AccessKind::Read)
            .expect_err("sibling is outside");
        if ws.parent() == sibling.parent() {
            assert!(
                err.contains("sibling") || err.contains("$HOME shorthand"),
                "{err}"
            );
        }
        let _ = std::fs::remove_dir_all(&sibling);
        let _ = std::fs::remove_dir_all(&ws);
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
        let outside = isolated_outside_dir();
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

    #[test]
    fn tilde_equals_home_not_workspace_relative() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        let home = dirs_home();
        let via_tilde = resolve_against_cwd(policy.cwd(), "~/.one/agent/models.json");
        let via_home = resolve_against_cwd(
            policy.cwd(),
            &format!("{}/.one/agent/models.json", home.display()),
        );
        assert!(
            paths_match(&via_tilde, &via_home),
            "tilde={via_tilde:?} home={via_home:?}"
        );
        // Must not land under the workspace as a literal `~` directory.
        assert!(
            !via_tilde.starts_with(&dir),
            "tilde must not be cwd-relative, got {via_tilde:?}"
        );

        policy
            .resolve("~/.one/agent/models.json", AccessKind::Read)
            .expect("agent models.json is readable via tilde");
        policy
            .resolve("~/.one/agent/models.json", AccessKind::Write)
            .expect("agent models.json is writable via tilde");

        let ssh_err = policy
            .resolve("~/.ssh/id_rsa", AccessKind::Read)
            .expect_err("~/.ssh stays outside");
        assert!(ssh_err.contains("outside workspace"), "{ssh_err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_config_writable_secrets_not() {
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        let agent = default_agent_dir();
        policy
            .check(&agent.join("models.json"), AccessKind::Write)
            .expect("models.json writable");
        policy
            .check(&agent.join("settings.json"), AccessKind::Write)
            .expect("settings.json writable");
        policy
            .check(&agent.join("mcp.json"), AccessKind::Write)
            .expect("mcp.json writable");
        let auth_err = policy
            .check(&agent.join("auth.json"), AccessKind::Write)
            .expect_err("auth.json must stay read-only");
        assert!(auth_err.contains("outside workspace"), "{auth_err}");
        assert!(
            auth_err.contains("models.json"),
            "deny should hint writable config files: {auth_err}"
        );
        policy
            .check(&agent.join("auth.json"), AccessKind::Read)
            .expect("auth.json is still readable under agent home");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_read_narrows_parent_of_readable_root() {
        let dir = temp_dir();
        let parent = isolated_outside_dir();
        let allowed = parent.join("allowed");
        let secret = parent.join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(allowed.join("hit.txt"), "ok").unwrap();
        std::fs::write(secret.join("leak.txt"), "no").unwrap();

        let policy = PathPolicy::workspace(dir.clone()).with_readable_root(allowed.clone());
        policy
            .check(&parent, AccessKind::Read)
            .expect_err("parent itself is not readable");

        let coverage = policy
            .resolve_read(parent.to_str().unwrap())
            .expect("parent of readable root should narrow");
        match coverage {
            ReadCoverage::Narrowed { roots, .. } => {
                assert_eq!(roots.len(), 1, "{roots:?}");
                assert!(paths_match(
                    &roots[0],
                    &normalize_existing_dir(allowed.clone())
                ));
            }
            other => panic!("expected Narrowed, got {other:?}"),
        }

        // Sibling of the readable root is still denied.
        policy
            .check(&secret.join("leak.txt"), AccessKind::Read)
            .expect_err("sibling must stay denied");

        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_write_path_allows_write_and_read() {
        let dir = temp_dir();
        let outside = isolated_outside_dir();
        let file = outside.join("note.txt");
        std::fs::write(&file, "x").unwrap();

        let policy = PathPolicy::workspace(dir.clone());
        policy
            .check(&file, AccessKind::Write)
            .expect_err("before grant");
        policy.grant_write_path(&file);
        policy
            .check(&file, AccessKind::Write)
            .expect("write grant covers the file");
        policy
            .check(&file, AccessKind::Read)
            .expect("write grant also grants read");

        let sibling = outside.join("other.txt");
        std::fs::write(&sibling, "y").unwrap();
        policy
            .check(&sibling, AccessKind::Write)
            .expect_err("sibling not granted");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_writable_root_covers_descendants() {
        let dir = temp_dir();
        let outside = isolated_outside_dir();
        let nested = outside.join("sub").join("a.rs");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "fn main() {}").unwrap();

        let policy = PathPolicy::workspace(dir.clone());
        policy.grant_writable_root(&outside);
        policy
            .check(&nested, AccessKind::Write)
            .expect("session write root covers descendants");
        policy
            .check(&nested, AccessKind::Read)
            .expect("session write root is also readable");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_time_capability_is_not_a_session_grant() {
        let dir = temp_dir();
        let outside = isolated_outside_dir();
        let file = outside.join("only-once.txt");
        std::fs::write(&file, "x").unwrap();
        let policy = PathPolicy::workspace(dir.clone());

        policy.grant_once("token", &file, AccessKind::Write);
        policy
            .check(&file, AccessKind::Write)
            .expect_err("ordinary checks must not inherit Once");
        policy
            .check_with_token(&file, AccessKind::Write, Some("token"))
            .expect("bound token works");
        policy
            .check_with_token(&file, AccessKind::Read, Some("token"))
            .expect_err("access kind is bound");
        policy.revoke_once("token");
        policy
            .check_with_token(&file, AccessKind::Write, Some("token"))
            .expect_err("reused token fails");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_policy_cwd_does_not_become_a_root() {
        let workspace = temp_dir();
        let outside = isolated_outside_dir();
        let child = PathPolicy::workspace(workspace.clone()).derive_for_child(outside.clone());
        child
            .check(&workspace.join("ok.txt"), AccessKind::Write)
            .expect("parent workspace remains writable");
        child
            .check(&outside.join("escape.txt"), AccessKind::Write)
            .expect_err("child cwd must not grant writes");
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn suggest_write_root_demotes_agent_home() {
        let agent = default_agent_dir();
        let models = agent.join("extra.json");
        assert!(
            suggest_read_root_impl(&models).is_some() || !agent.is_dir(),
            "read may suggest agent dir when it exists"
        );
        let dir = temp_dir();
        let policy = PathPolicy::workspace(dir.clone());
        assert!(
            policy.suggest_write_root(&models).is_none(),
            "write session root must not unlock ~/.one/agent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
