//! OS-level bash sandbox via bubblewrap (`bwrap`).
//!
//! Aligned with OpenAI Codex Linux `workspace-write` layout
//! (`codex-rs/linux-sandbox` bwrap filesystem args):
//!
//! 1. **Entire host FS read-only** — `--ro-bind / /`  
//!    (so `/etc/alternatives`, toolchains, ssl, etc. all resolve)
//! 2. Minimal `--dev` / `--proc`
//! 3. **Writable roots** rebound on top:
//!    - policy workspace roots + cwd
//!    - `/tmp` and `$TMPDIR` (Codex workspace-write defaults)
//! 4. Network shared (no `--unshare-net`) so package managers work
//!
//! Sensitive agent config dirs under a writable root are re-masked RO when
//! present (`.codex`, `.one`, `.agents`) — not project `.git`, so `git commit`
//! inside a workspace still works.
//!
//! `full-access` disables wrapping. If `bwrap` is missing, commands run
//! unsandboxed with a one-time warning (best-effort).
//!
//! Override: `ONE_BASH_SANDBOX=0` disables the OS sandbox only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::path_policy::{PathPolicy, SandboxMode};

static WARNED_NO_BWRAP: AtomicBool = AtomicBool::new(false);

/// Dir names re-applied as RO when they sit under a writable root.
///
/// Matches Codex-style protection of agent metadata; **excludes** `.git` so
/// normal VCS writes in a project workspace keep working.
const PROTECTED_UNDER_WRITABLE: &[&str] = &[".codex", ".one", ".agents"];

/// How to launch a bash command under the host OS.
#[derive(Debug, Clone)]
pub struct OsSandbox {
    pub enabled: bool,
    pub cwd: PathBuf,
    pub writable_roots: Vec<PathBuf>,
    pub readable_roots: Vec<PathBuf>,
}

impl OsSandbox {
    pub fn from_policy(policy: &PathPolicy) -> Self {
        let enabled = match policy.mode() {
            SandboxMode::WorkspaceWrite => true,
            SandboxMode::FullAccess => false,
        };
        // Env override: ONE_BASH_SANDBOX=0 disables OS sandbox only.
        let enabled = enabled
            && std::env::var("ONE_BASH_SANDBOX")
                .map(|v| v != "0" && v != "false" && v != "off")
                .unwrap_or(true);

        Self {
            enabled,
            cwd: policy.cwd().to_path_buf(),
            writable_roots: policy.writable_roots().map(|p| p.to_path_buf()).collect(),
            readable_roots: policy.readable_roots().map(|p| p.to_path_buf()).collect(),
        }
    }

    pub fn disabled(cwd: impl Into<PathBuf>) -> Self {
        Self {
            enabled: false,
            cwd: cwd.into(),
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
        }
    }

    pub fn bwrap_available() -> bool {
        which_bwrap().is_some()
    }

    /// Build `(program, args)` for `tokio::process::Command`.
    ///
    /// Always ends with running the user command via `bash -lc`.
    pub fn command_line(&self, user_command: &str) -> (String, Vec<String>) {
        if !self.enabled {
            return ("bash".into(), vec!["-lc".into(), user_command.to_string()]);
        }
        let Some(bwrap) = which_bwrap() else {
            if !WARNED_NO_BWRAP.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "one: warning: bwrap not found — bash OS sandbox disabled \
                     (install bubblewrap, or set ONE_BASH_SANDBOX=0 to silence)"
                );
            }
            return ("bash".into(), vec!["-lc".into(), user_command.to_string()]);
        };

        let mut args = Vec::new();

        // --- Codex-style base: whole filesystem RO, then carve RW roots ---
        // `--ro-bind / /` keeps /etc/alternatives, ld.so paths, etc. intact
        // (the old selective-mount list broke `cc` → /etc/alternatives/cc).
        args.push("--ro-bind".into());
        args.push("/".into());
        args.push("/".into());

        // Minimal device tree + proc (after RO root, before writable binds).
        args.push("--dev".into());
        args.push("/dev".into());
        args.push("--proc".into());
        args.push("/proc".into());

        // Collect writable roots (dedupe by canonical-ish display string).
        let mut writables: Vec<PathBuf> = Vec::new();
        let mut push_w = |p: PathBuf| {
            if !p.as_os_str().is_empty() && !writables.iter().any(|e| e == &p) {
                writables.push(p);
            }
        };

        for w in &self.writable_roots {
            push_w(w.clone());
        }
        push_w(self.cwd.clone());

        // Codex workspace-write: /tmp + $TMPDIR are writable by default.
        push_w(PathBuf::from("/tmp"));
        if let Some(td) = std::env::var_os("TMPDIR") {
            let td = PathBuf::from(td);
            if td.as_os_str() != "/tmp" && (td.is_dir() || td.parent().is_some()) {
                push_w(td);
            }
        }
        // Common secondary temp root (cargo / some tools).
        if Path::new("/var/tmp").is_dir() {
            push_w(PathBuf::from("/var/tmp"));
        }

        for w in &writables {
            push_bind_try_path(&mut args, w);
        }

        // Extra readable roots are already covered by RO `/` — no-op unless
        // we later switch to a minimal (non full-disk-read) profile. Keep the
        // field for API compatibility; explicit RO re-bind is harmless.
        for r in &self.readable_roots {
            if !writables.iter().any(|w| w == r) {
                push_ro_try_path(&mut args, r);
            }
        }

        // Re-mask agent metadata dirs under writable roots (not project .git).
        for w in &writables {
            for name in PROTECTED_UNDER_WRITABLE {
                let p = w.join(name);
                if p.exists() {
                    push_ro_try_path(&mut args, &p);
                }
            }
        }

        args.push("--chdir".into());
        args.push(self.cwd.display().to_string());
        args.push("--die-with-parent".into());
        // Keep network (do not --unshare-net).
        args.push("--".into());
        args.push("bash".into());
        args.push("-lc".into());
        args.push(user_command.to_string());

        (bwrap, args)
    }
}

fn which_bwrap() -> Option<String> {
    if Path::new("/usr/bin/bwrap").is_file() {
        return Some("/usr/bin/bwrap".into());
    }
    if Path::new("/bin/bwrap").is_file() {
        return Some("/bin/bwrap".into());
    }
    // PATH lookup
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = Path::new(dir).join("bwrap");
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
    }
    None
}

fn push_ro_try_path(args: &mut Vec<String>, path: &Path) {
    let s = path.display().to_string();
    args.push("--ro-bind-try".into());
    args.push(s.clone());
    args.push(s);
}

fn push_bind_try_path(args: &mut Vec<String>, path: &Path) {
    let s = path.display().to_string();
    args.push("--bind-try".into());
    args.push(s.clone());
    args.push(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_policy::PathPolicy;

    fn unique_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn full_access_disables() {
        let policy = PathPolicy::full_access(std::env::temp_dir());
        let sb = OsSandbox::from_policy(&policy);
        assert!(!sb.enabled);
        let (prog, args) = sb.command_line("echo hi");
        assert_eq!(prog, "bash");
        assert_eq!(args, vec!["-lc".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn workspace_wraps_when_bwrap_present() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        let dir = unique_dir("one-os-sb");
        let policy = PathPolicy::workspace(dir.clone());
        let mut sb = OsSandbox::from_policy(&policy);
        sb.enabled = true;
        let (prog, args) = sb.command_line("echo hi");
        assert!(prog.contains("bwrap"), "{prog}");
        assert!(args.iter().any(|a| a == "--die-with-parent"));
        // Codex-style full-disk RO root.
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/" && w[2] == "/"),
            "expected --ro-bind / / in {args:?}"
        );
        assert!(args.iter().any(|a| a == "echo hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bwrap_blocks_write_outside_workspace() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        // Workspace under /tmp (writable). Leak target under $HOME (RO via /).
        let dir = unique_dir("one-os-sb-block");
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let outside = home.join(format!(
            ".one-os-sb-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&outside);

        let mut sb = OsSandbox::from_policy(&PathPolicy::workspace(dir.clone()));
        sb.enabled = true;
        let (prog, args) = sb.command_line(&format!("echo leaked > '{}'", outside.display()));
        let out = std::process::Command::new(&prog)
            .args(&args)
            .current_dir(&dir)
            .output()
            .expect("spawn bwrap");
        let leaked = outside.exists();
        if leaked {
            let _ = std::fs::remove_file(&outside);
        }
        assert!(
            !leaked,
            "sandbox allowed write outside workspace into $HOME; status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bwrap_resolves_cc_via_etc_alternatives() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        // Host must have the Debian/Ubuntu alternatives layout.
        if !Path::new("/etc/alternatives/cc").exists() && !Path::new("/usr/bin/cc").exists() {
            return;
        }
        let dir = unique_dir("one-os-sb-cc");
        let mut sb = OsSandbox::from_policy(&PathPolicy::workspace(dir.clone()));
        sb.enabled = true;
        // Prove alternatives + compiler are visible (the old selective /etc mount failed here).
        let (prog, args) = sb.command_line(
            "test -e /usr/bin/cc && command -v cc >/dev/null && cc -dumpversion >/dev/null",
        );
        let out = std::process::Command::new(&prog)
            .args(&args)
            .current_dir(&dir)
            .output()
            .expect("spawn bwrap");
        assert!(
            out.status.success(),
            "cc must work inside sandbox (Codex-style / mount); status={:?} stdout={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bwrap_allows_write_in_workspace() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        let dir = unique_dir("one-os-sb-rw");
        let target = dir.join("ok.txt");
        let mut sb = OsSandbox::from_policy(&PathPolicy::workspace(dir.clone()));
        sb.enabled = true;
        let (prog, args) = sb.command_line(&format!("echo hi > '{}'", target.display()));
        let out = std::process::Command::new(&prog)
            .args(&args)
            .current_dir(&dir)
            .output()
            .expect("spawn bwrap");
        assert!(
            out.status.success() && target.is_file(),
            "workspace write failed; status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
