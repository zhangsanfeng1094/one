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
            "require_escalated"
            | "escalated"
            | "escalate"
            | "full_access"
            | "full-access"
            | "danger-full-access"
            | "unsandboxed" => Some(Self::RequireEscalated),
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

/// Heuristic: sandboxed run failed in a way that often means the sandbox
/// blocked a legitimate host action (Codex `escalate_on_failure` analogue).
///
/// We intentionally do **not** escalate on ordinary non-zero exits (tests,
/// grep no-match, etc.).
pub fn looks_like_sandbox_denial(exit_code: Option<i32>, combined_output: &str) -> bool {
    // bwrap / seccomp often kills the process with a signal (no exit code).
    if exit_code.is_none() {
        return true;
    }
    let lower = combined_output.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "operation not permitted",
        "permission denied",
        "read-only file system",
        "readonly file system",
        "cannot kill pid",
        "not permitted",
        "eperm",
        "erofs",
        "sandbox",
        "bwrap:",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
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
        assert!(!looks_like_sandbox_denial(Some(1), "grep: no matches"));
        assert!(!looks_like_sandbox_denial(Some(0), "ok"));
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
