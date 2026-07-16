//! OS-level bash sandbox via bubblewrap (`bwrap`).
//!
//! When enabled (default for `workspace-write`), shell commands run inside a
//! minimal filesystem namespace:
//! - system paths (`/usr`, `/bin`, …) read-only
//! - `$HOME` read-only (cargo/npm caches, rustup)
//! - workspace + `--add-dir` roots **read-write**
//! - skill roots (`~/.one/agent`, `~/.agents/skills`, compat `~/.codex/skills`, …) RO
//! - network shared (no `--unshare-net`) so package managers work
//!
//! `full-access` disables wrapping. If `bwrap` is missing, commands run unsandboxed
//! with a one-time warning via stderr (best-effort).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::path_policy::{PathPolicy, SandboxMode};

static WARNED_NO_BWRAP: AtomicBool = AtomicBool::new(false);

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
            writable_roots: policy
                .writable_roots()
                .map(|p| p.to_path_buf())
                .collect(),
            readable_roots: policy
                .readable_roots()
                .map(|p| p.to_path_buf())
                .collect(),
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
            return (
                "bash".into(),
                vec!["-lc".into(), user_command.to_string()],
            );
        }
        let Some(bwrap) = which_bwrap() else {
            if !WARNED_NO_BWRAP.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "one: warning: bwrap not found — bash OS sandbox disabled \
                     (install bubblewrap, or set ONE_BASH_SANDBOX=0 to silence)"
                );
            }
            return (
                "bash".into(),
                vec!["-lc".into(), user_command.to_string()],
            );
        };

        let mut args = Vec::new();

        // Base system (RO).
        for p in [
            "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/opt",
        ] {
            push_ro_try(&mut args, p);
        }
        // DNS / TLS / user identity (RO).
        for p in [
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/ssl",
            "/etc/ca-certificates",
            "/etc/pki",
            "/etc/passwd",
            "/etc/group",
            "/etc/localtime",
            "/etc/timezone",
            "/etc/gitconfig",
        ] {
            push_ro_try(&mut args, p);
        }
        // Devices & proc for normal tooling.
        args.push("--dev".into());
        args.push("/dev".into());
        args.push("--proc".into());
        args.push("/proc".into());
        // Private temp — do NOT bind host /tmp (that would allow write-escape via /tmp).
        args.push("--tmpfs".into());
        args.push("/tmp".into());
        args.push("--tmpfs".into());
        args.push("/var/tmp".into());

        // Home RO so cargo/rustup/npm work; writable roots rebound RW below.
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            if home.exists() {
                push_ro_try_path(&mut args, &home);
            }
        }

        // Extra readable roots (agent dir, etc.).
        for r in &self.readable_roots {
            push_ro_try_path(&mut args, r);
        }

        // Writable workspace roots (override any RO home bind).
        for w in &self.writable_roots {
            push_bind_try_path(&mut args, w);
        }
        // Always ensure cwd is writable when sandboxed.
        push_bind_try_path(&mut args, &self.cwd);

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

fn push_ro_try(args: &mut Vec<String>, path: &str) {
    args.push("--ro-bind-try".into());
    args.push(path.into());
    args.push(path.into());
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
        let dir = std::env::temp_dir().join(format!("one-os-sb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = PathPolicy::workspace(dir.clone());
        // Force-enable even if env disables.
        let mut sb = OsSandbox::from_policy(&policy);
        sb.enabled = true;
        let (prog, args) = sb.command_line("echo hi");
        assert!(prog.contains("bwrap"), "{prog}");
        assert!(args.iter().any(|a| a == "--die-with-parent"));
        assert!(args.iter().any(|a| a == "echo hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bwrap_blocks_write_outside_workspace() {
        if !OsSandbox::bwrap_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "one-os-sb-block-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join(format!(
            "one-os-sb-outside-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&outside);

        let mut sb = OsSandbox::from_policy(&PathPolicy::workspace(dir.clone()));
        sb.enabled = true;
        let (prog, args) = sb.command_line(&format!(
            "echo leaked > {}",
            outside.display()
        ));
        let out = std::process::Command::new(&prog)
            .args(&args)
            .current_dir(&dir)
            .output()
            .expect("spawn bwrap");
        // Either command fails or file was not created outside.
        let leaked = outside.exists();
        if leaked {
            let _ = std::fs::remove_file(&outside);
        }
        assert!(
            !leaked,
            "sandbox allowed write outside workspace; status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
