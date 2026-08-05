//! Per-command sandbox override (Codex-aligned).
//!
//! Mirrors Codex `SandboxPermissions`:
//! - `use_default` — session PathPolicy / bwrap unchanged
//! - `require_escalated` — request to run **outside** the OS sandbox
//!
//! `with_additional_permissions` is intentionally not implemented yet
//! (one has no granular network/FS permission profiles).

use one_core::tool::ToolCall;
use serde_json::Value;

/// Codex `sandbox_permissions` enum (snake_case in JSON).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SandboxPermissions {
    /// Run with the session sandbox policy unchanged.
    #[default]
    UseDefault,
    /// Request to run outside the OS sandbox (bubblewrap off for this call).
    RequireEscalated,
}

impl SandboxPermissions {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UseDefault => "use_default",
            Self::RequireEscalated => "require_escalated",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "use_default" | "default" | "workspace" | "workspace_write" | "workspace-write" => {
                Some(Self::UseDefault)
            }
            "require_escalated" | "escalated" | "escalate" | "full_access" | "full-access"
            | "danger-full-access" | "unsandboxed" => Some(Self::RequireEscalated),
            // Recognized but not supported — treat as escalate request so we
            // still hit the approval path rather than silently ignoring.
            "with_additional_permissions" | "additional" => Some(Self::RequireEscalated),
            _ => None,
        }
    }

    pub fn from_value(v: Option<&Value>) -> Self {
        v.and_then(|v| v.as_str())
            .and_then(Self::parse)
            .unwrap_or(Self::UseDefault)
    }
}

/// Read `sandbox_permissions` from a tool call (bash/shell).
pub fn sandbox_permissions_of(call: &ToolCall) -> SandboxPermissions {
    SandboxPermissions::from_value(call.arguments.get("sandbox_permissions"))
}

/// Optional user-facing justification for escalate prompts (Codex field).
pub fn justification_of(call: &ToolCall) -> Option<String> {
    call.arguments
        .get("justification")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Whether this call is requesting OS-sandbox escape.
pub fn requires_escalation(call: &ToolCall) -> bool {
    matches!(
        sandbox_permissions_of(call),
        SandboxPermissions::RequireEscalated
    )
}

/// Heuristic: sandboxed run failed in a way that often means the **OS** sandbox
/// (bubblewrap) blocked a legitimate host action (Codex `escalate_on_failure`).
///
/// This is **not** the workspace PathPolicy. Ordinary non-zero exits (tests,
/// `cargo fmt --check` diffs, grep no-match) must **not** match.
///
/// Markers are OS-error phrases only. Bare substrings like `"sandbox"` used to
/// false-positive on rustc/fmt diffs and tests that mention sandbox in source
/// (e.g. `SandboxEscalate`, `sandbox_permissions.rs`) and then pop a useless
/// "run outside bwrap?" prompt.
pub fn looks_like_sandbox_denial(exit_code: Option<i32>, combined_output: &str) -> bool {
    // bwrap / seccomp often kills the process with a signal (no exit code).
    if exit_code.is_none() {
        return true;
    }
    let lower = combined_output.to_ascii_lowercase();
    // Prefer full OS / bwrap phrases over single tokens. Avoid:
    // - "sandbox" (source/docs/test names)
    // - bare "eperm"/"erofs" (identifiers, hex dumps)
    // - bare "not permitted" (docs prose without OS error context)
    const MARKERS: &[&str] = &[
        "operation not permitted",
        "permission denied",
        "read-only file system",
        "readonly file system",
        "read-only filesystem",
        "readonly filesystem",
        "cannot kill pid",
        // bwrap emits errors as `bwrap: …`
        "bwrap:",
        // Toolchain resolution failures that historically meant incomplete mounts
        // (Codex-style full-disk RO root usually avoids these).
        "linker `cc` not found",
        "linker 'cc' not found",
        "linker cc not found",
    ];
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // Whole-word EPERM / EROFS (errno names), not substrings inside identifiers.
    contains_errno_token(&lower, "eperm") || contains_errno_token(&lower, "erofs")
}

/// True when `token` appears as its own word (not inside `eperm_helper` etc.).
fn contains_errno_token(haystack_lower: &str, token: &str) -> bool {
    let bytes = haystack_lower.as_bytes();
    let t = token.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0usize;
    while let Some(rel) = haystack_lower[start..].find(token) {
        let i = start + rel;
        let before_ok = i == 0 || !is_ident(bytes[i - 1]);
        let after = i + t.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = i + t.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash_call(args: Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: args,
        }
    }

    #[test]
    fn parse_variants() {
        assert_eq!(
            SandboxPermissions::parse("require_escalated"),
            Some(SandboxPermissions::RequireEscalated)
        );
        assert_eq!(
            SandboxPermissions::parse("use_default"),
            Some(SandboxPermissions::UseDefault)
        );
        assert_eq!(
            SandboxPermissions::from_value(Some(&json!("require_escalated"))),
            SandboxPermissions::RequireEscalated
        );
    }

    #[test]
    fn denial_heuristic() {
        assert!(looks_like_sandbox_denial(None, ""));
        assert!(looks_like_sandbox_denial(
            Some(1),
            "kill: Operation not permitted"
        ));
        assert!(looks_like_sandbox_denial(
            Some(1),
            "bash: /root/secret: Permission denied"
        ));
        assert!(looks_like_sandbox_denial(
            Some(1),
            "touch: cannot touch '/home/x/.cache/x': Read-only file system"
        ));
        assert!(looks_like_sandbox_denial(Some(1), "bwrap: loopback: …"));
        assert!(looks_like_sandbox_denial(
            Some(1),
            "error: linking with `cc` failed: exit status: 1\n  = note: linker `cc` not found"
        ));
        // Whole-word errno only.
        assert!(looks_like_sandbox_denial(Some(1), "open failed: EPERM"));
        assert!(!looks_like_sandbox_denial(
            Some(1),
            "fn eperm_helper() {}" // identifier, not errno
        ));
        assert!(!looks_like_sandbox_denial(Some(1), "grep: no matches"));
        assert!(!looks_like_sandbox_denial(Some(0), "ok"));
    }

    #[test]
    fn denial_heuristic_ignores_source_mentions_of_sandbox() {
        // Regression: cargo fmt --check / cargo test print project sources and
        // test names that contain "sandbox" / "SandboxEscalate" / path
        // `sandbox_permissions.rs`. Those must not trigger escalate_on_failure.
        let fmt_like = r#"
Diff in /home/fxh/tools/one/crates/one-cli/src/approval.rs:244:
                 PendingKind::Standard | PendingKind::SandboxEscalate => {
-                    let escalate =
-                        matches!(pending.kind, PendingKind::SandboxEscalate);
+                    let escalate = matches!(pending.kind, PendingKind::SandboxEscalate);
Diff in /home/fxh/tools/one/crates/one-tools/src/sandbox_permissions.rs:1:
 //! Per-command sandbox override (Codex-aligned).
"#;
        assert!(
            !looks_like_sandbox_denial(Some(1), fmt_like),
            "fmt diffs mentioning sandbox must not look like OS denial"
        );

        let test_like = r#"
running 12 tests
test sandbox_mode_workspace_write ... ok
test require_escalated_can_write_outside_workspace ... ok
test result: FAILED. 11 passed; 1 failed
"#;
        assert!(
            !looks_like_sandbox_denial(Some(101), test_like),
            "test names with sandbox must not look like OS denial"
        );

        // Prose without a real OS error phrase.
        assert!(!looks_like_sandbox_denial(
            Some(1),
            "this action is not permitted by policy"
        ));
    }

    #[test]
    fn requires_from_call() {
        let c = bash_call(json!({
            "command": "kill 1",
            "sandbox_permissions": "require_escalated",
            "justification": "clean up host processes"
        }));
        assert!(requires_escalation(&c));
        assert_eq!(
            justification_of(&c).as_deref(),
            Some("clean up host processes")
        );
    }
}
