//! Fine-grained permission rules (Claude-style allow / deny / ask).
//!
//! Rule syntax:
//! - `Tool` — all uses of the tool (e.g. `Bash`, `Write`)
//! - `Tool(specifier)` — scoped match (e.g. `Bash(git push *)`, `Edit(**/.env*)`)
//!
//! Evaluation order: **deny → ask → allow → built-in defaults**.

use one_core::tool::ToolCall;
use serde::{Deserialize, Serialize};

/// Outcome of evaluating rules + defaults (before interactive resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Deny { reason: String },
    /// Needs user confirmation (interactive) or fail-closed (print/RPC).
    Ask { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Deny,
    Ask,
}

/// One permission rule: action + tool pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: RuleAction,
    /// Raw rule text as written by the user, e.g. `Bash(git push *)`.
    pub rule: String,
}

impl PermissionRule {
    pub fn parse(action: RuleAction, raw: &str) -> Option<Self> {
        let rule = raw.trim().to_string();
        if rule.is_empty() {
            return None;
        }
        // Validate shape early.
        let _ = ParsedRule::parse(&rule)?;
        Some(Self { action, rule })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
}

impl PermissionRules {
    pub fn compiled(&self) -> Vec<PermissionRule> {
        let mut out = Vec::new();
        for r in &self.deny {
            if let Some(p) = PermissionRule::parse(RuleAction::Deny, r) {
                out.push(p);
            }
        }
        for r in &self.ask {
            if let Some(p) = PermissionRule::parse(RuleAction::Ask, r) {
                out.push(p);
            }
        }
        for r in &self.allow {
            if let Some(p) = PermissionRule::parse(RuleAction::Allow, r) {
                out.push(p);
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.ask.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ParsedRule {
    /// Lowercase tool name, or `*` for any tool.
    tool: String,
    /// Optional specifier (command / path pattern).
    specifier: Option<String>,
}

impl ParsedRule {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(open) = raw.find('(') {
            if !raw.ends_with(')') {
                return None;
            }
            let tool = raw[..open].trim().to_ascii_lowercase();
            let inner = raw[open + 1..raw.len() - 1].trim();
            if tool.is_empty() {
                return None;
            }
            Some(Self {
                tool,
                specifier: if inner.is_empty() || inner == "*" {
                    None
                } else {
                    Some(inner.to_string())
                },
            })
        } else {
            Some(Self {
                tool: raw.to_ascii_lowercase(),
                specifier: None,
            })
        }
    }

    fn matches(&self, call: &ToolCall) -> bool {
        let name = call.name.to_ascii_lowercase();
        if self.tool != "*" && self.tool != name {
            return false;
        }
        let Some(spec) = &self.specifier else {
            return true;
        };
        let subject = match name.as_str() {
            "bash" | "shell" => call
                .arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "read" | "write" | "edit" | "grep" | "find" | "ls" => call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "web_fetch" => call
                .arguments
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            _ => {
                // Generic: match against compact JSON args.
                &call.arguments.to_string()
            }
        };
        wildcard_match(spec, subject)
    }
}

/// Glob-like match: `*` matches any sequence (including empty / spaces).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    wildcard_match_inner(pattern.as_bytes(), text.as_bytes())
}

fn wildcard_match_inner(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == text[ti] || pat[pi] == b'?') {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Command string from a bash/shell tool call, if present.
pub fn bash_command(call: &ToolCall) -> Option<&str> {
    match call.name.as_str() {
        "bash" | "shell" => call.arguments.get("command").and_then(|v| v.as_str()),
        _ => None,
    }
}

/// Codex-style session prefix for "don't ask again for commands starting with …".
///
/// Heuristic (keeps approval narrower than full Always):
/// - wrappers (`sudo`, `doas`, …) + next non-flag token when present
/// - multi-word CLIs (`git`, `cargo`, `npm`, …) → first two tokens when 2nd is not a flag
/// - otherwise first token only
///
/// Returns `None` for non-bash tools or empty commands.
pub fn suggested_command_prefix(call: &ToolCall) -> Option<String> {
    let cmd = bash_command(call)?.trim();
    if cmd.is_empty() {
        return None;
    }
    suggested_command_prefix_from_cmd(cmd)
}

/// Same as [`suggested_command_prefix`] but from a raw command string (tests / UI).
pub fn suggested_command_prefix_from_cmd(command: &str) -> Option<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }

    const WRAPPERS: &[&str] = &[
        "sudo", "doas", "nice", "nohup", "time", "command", "builtin", "exec",
    ];
    const MULTI: &[&str] = &[
        "git", "cargo", "npm", "pnpm", "yarn", "bun", "pip", "pip3", "docker", "kubectl", "gh",
        "systemctl", "apt", "apt-get", "brew", "podman", "terraform", "aws", "gcloud", "go",
        "make", "cmake", "mvn", "gradle", "poetry", "uv", "rustup", "npx", "deno",
    ];

    let mut i = 0usize;
    // Skip leading ENV=value assignments: `FOO=1 cargo test`
    while i < tokens.len()
        && tokens[i].contains('=')
        && !tokens[i].starts_with('-')
        && !tokens[i].starts_with('/')
    {
        i += 1;
    }
    if i >= tokens.len() {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    let head = tokens[i];
    parts.push(head);
    i += 1;

    if WRAPPERS.iter().any(|w| head.eq_ignore_ascii_case(w)) {
        // sudo apt install … → "sudo apt"
        while i < tokens.len() && tokens[i].starts_with('-') {
            // keep flags out of the prefix (sudo -u root … is too variable)
            break;
        }
        if i < tokens.len() && !tokens[i].starts_with('-') {
            parts.push(tokens[i]);
        }
    } else if MULTI.iter().any(|m| head.eq_ignore_ascii_case(m))
        && i < tokens.len()
        && !tokens[i].starts_with('-')
    {
        parts.push(tokens[i]);
    }

    let prefix = parts.join(" ");
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// True when `command` is exactly `prefix` or continues after a word boundary.
///
/// `cargo test` matches `cargo test` and `cargo test --quiet`, not `cargo testing`.
pub fn command_matches_prefix(command: &str, prefix: &str) -> bool {
    let cmd = command.trim_start();
    let p = prefix.trim();
    if p.is_empty() {
        return false;
    }
    if cmd == p {
        return true;
    }
    let mut bound = String::with_capacity(p.len() + 1);
    bound.push_str(p);
    bound.push(' ');
    cmd.starts_with(&bound)
}

/// Fingerprint for session-level "always allow this exact call".
///
/// Escalated bash calls use a separate key (`bash::escalate::{cmd}`) so that
/// approving a high-risk command under the sandbox does not auto-approve
/// unsandboxed re-runs (Codex session escalate is scoped to the escalate path).
///
/// For prefix-family allows, see [`suggested_command_prefix`] + session prefix list
/// on the permission gate (not this fingerprint).
pub fn call_fingerprint(call: &ToolCall) -> String {
    let subject = match call.name.as_str() {
        "bash" | "shell" => call
            .arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "write" | "edit" | "read" => call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => call.arguments.to_string(),
    };
    let name = call.name.to_ascii_lowercase();
    if matches!(name.as_str(), "bash" | "shell")
        && crate::sandbox_permissions::requires_escalation(call)
    {
        format!("{name}::escalate::{subject}")
    } else {
        format!("{name}::{subject}")
    }
}

/// Human-readable summary for approval UI.
///
/// Prefer a short `description` when the model provided one; otherwise the
/// command (callers / TUI may further truncate for display).
pub fn call_summary(call: &ToolCall) -> String {
    match call.name.as_str() {
        "bash" | "shell" => {
            let desc = call
                .arguments
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let cmd = call
                .arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("(empty command)");
            // Prefer human description for the dock; keep command for escalate
            // body via the same field (format_escalate_body peels prefixes).
            let core = desc.unwrap_or(cmd);
            if crate::sandbox_permissions::requires_escalation(call) {
                // Prefer command for escalate preview (user should see what runs).
                // Still prefix so the UI can detect escalate-shaped summaries.
                format!("[outside sandbox] {cmd}")
            } else {
                core.to_string()
            }
        }
        "write" | "edit" | "read" | "ls" | "grep" | "find" => {
            let path = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            format!("{} {path}", call.name)
        }
        other => format!("{other} {}", call.arguments),
    }
}

/// Evaluate configured rules + safe defaults.
///
/// `auto_approve` skips built-in high-risk bash asks (still respects explicit deny).
pub fn evaluate(
    call: &ToolCall,
    rules: &[PermissionRule],
    auto_approve: bool,
) -> PermissionVerdict {
    // Separate by action while preserving config order within each class.
    let mut denials = Vec::new();
    let mut asks = Vec::new();
    let mut allows = Vec::new();
    for r in rules {
        match r.action {
            RuleAction::Deny => denials.push(r),
            RuleAction::Ask => asks.push(r),
            RuleAction::Allow => allows.push(r),
        }
    }

    for r in denials {
        if let Some(parsed) = ParsedRule::parse(&r.rule) {
            if parsed.matches(call) {
                return PermissionVerdict::Deny {
                    reason: format!("denied by rule `{}`", r.rule),
                };
            }
        }
    }
    for r in asks {
        if let Some(parsed) = ParsedRule::parse(&r.rule) {
            if parsed.matches(call) {
                if auto_approve {
                    return PermissionVerdict::Allow;
                }
                return PermissionVerdict::Ask {
                    reason: format!("ask rule `{}`", r.rule),
                };
            }
        }
    }
    for r in allows {
        if let Some(parsed) = ParsedRule::parse(&r.rule) {
            if parsed.matches(call) {
                return PermissionVerdict::Allow;
            }
        }
    }

    // Built-in defaults.
    default_verdict(call, auto_approve)
}

fn default_verdict(call: &ToolCall, auto_approve: bool) -> PermissionVerdict {
    match call.name.as_str() {
        "read" | "grep" | "find" | "ls" | "bash_output" | "bash_kill" | "web_search"
        | "web_fetch" | "exit_plan_mode" => PermissionVerdict::Allow,
        "write" | "edit" => PermissionVerdict::Allow, // PathPolicy enforces workspace
        "bash" | "shell" => {
            let command = call
                .arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Some(pat) = crate::sandbox::is_command_blocked(command) {
                return PermissionVerdict::Deny {
                    reason: format!("blocked command pattern: {pat}"),
                };
            }

            // Codex-aligned: model-requested sandbox escape always needs Ask
            // (unless auto_approve / --yes). Distinct reason prefix drives TUI copy.
            if crate::sandbox_permissions::requires_escalation(call) {
                if auto_approve {
                    return PermissionVerdict::Allow;
                }
                let just = crate::sandbox_permissions::justification_of(call)
                    .unwrap_or_else(|| "model requested unsandboxed execution".into());
                return PermissionVerdict::Ask {
                    reason: format!("sandbox escalation: {just}"),
                };
            }

            if !auto_approve {
                if let Some(pat) = crate::sandbox::requires_confirmation(command) {
                    return PermissionVerdict::Ask {
                        reason: format!("high-risk bash pattern `{pat}`"),
                    };
                }
            }
            PermissionVerdict::Allow
        }
        _ => PermissionVerdict::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(cmd: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({ "command": cmd }),
        }
    }

    fn write(path: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "write".into(),
            arguments: json!({ "path": path, "content": "x" }),
        }
    }

    #[test]
    fn wildcard_basics() {
        assert!(wildcard_match("git push *", "git push origin main"));
        assert!(wildcard_match("cargo *", "cargo test -p one"));
        assert!(!wildcard_match("git push *", "git status"));
        assert!(wildcard_match("**/.env*", "crates/app/.env.local"));
    }

    #[test]
    fn deny_beats_allow() {
        let rules = vec![
            PermissionRule::parse(RuleAction::Allow, "Bash(git *)").unwrap(),
            PermissionRule::parse(RuleAction::Deny, "Bash(git push *)").unwrap(),
        ];
        let v = evaluate(&bash("git push origin main"), &rules, false);
        assert!(matches!(v, PermissionVerdict::Deny { .. }), "{v:?}");
    }

    #[test]
    fn allow_cargo() {
        let rules = vec![PermissionRule::parse(RuleAction::Allow, "Bash(cargo *)").unwrap()];
        let v = evaluate(&bash("cargo test"), &rules, false);
        assert_eq!(v, PermissionVerdict::Allow);
    }

    #[test]
    fn default_high_risk_asks() {
        let v = evaluate(&bash("sudo apt update"), &[], false);
        assert!(matches!(v, PermissionVerdict::Ask { .. }), "{v:?}");
        let v2 = evaluate(&bash("sudo apt update"), &[], true);
        assert_eq!(v2, PermissionVerdict::Allow);
    }

    #[test]
    fn default_blocks_rm_root() {
        let v = evaluate(&bash("rm -rf /"), &[], true);
        assert!(matches!(v, PermissionVerdict::Deny { .. }), "{v:?}");
    }

    #[test]
    fn ask_rule_for_write_env() {
        let rules = vec![PermissionRule::parse(RuleAction::Ask, "Write(**/.env*)").unwrap()];
        let v = evaluate(&write("app/.env"), &rules, false);
        assert!(matches!(v, PermissionVerdict::Ask { .. }), "{v:?}");
    }

    #[test]
    fn require_escalated_asks_even_for_safe_commands() {
        let call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({
                "command": "echo hi",
                "sandbox_permissions": "require_escalated",
                "justification": "need host access"
            }),
        };
        let v = evaluate(&call, &[], false);
        match v {
            PermissionVerdict::Ask { reason } => {
                assert!(reason.starts_with("sandbox escalation:"), "{reason}");
                assert!(reason.contains("need host access"), "{reason}");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
        // auto_approve skips the prompt (like -y / always-approve).
        assert_eq!(evaluate(&call, &[], true), PermissionVerdict::Allow);
    }

    #[test]
    fn escalate_fingerprint_differs() {
        let normal = bash("kill 1");
        let escalated = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({
                "command": "kill 1",
                "sandbox_permissions": "require_escalated"
            }),
        };
        assert_ne!(call_fingerprint(&normal), call_fingerprint(&escalated));
        assert!(call_fingerprint(&escalated).contains("escalate"));
    }

    #[test]
    fn suggested_prefix_git_and_cargo() {
        assert_eq!(
            suggested_command_prefix_from_cmd("git push origin main").as_deref(),
            Some("git push")
        );
        assert_eq!(
            suggested_command_prefix_from_cmd("cargo test --quiet").as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            suggested_command_prefix_from_cmd("sudo apt install foo").as_deref(),
            Some("sudo apt")
        );
        assert_eq!(
            suggested_command_prefix_from_cmd("rm -rf /tmp/x").as_deref(),
            Some("rm")
        );
        assert_eq!(
            suggested_command_prefix_from_cmd("FOO=1 cargo build").as_deref(),
            Some("cargo build")
        );
    }

    #[test]
    fn command_prefix_word_boundary() {
        assert!(command_matches_prefix("cargo test --quiet", "cargo test"));
        assert!(command_matches_prefix("cargo test", "cargo test"));
        assert!(!command_matches_prefix("cargo testing", "cargo test"));
        assert!(!command_matches_prefix("cargotest", "cargo"));
        assert!(command_matches_prefix("cargo", "cargo"));
    }

    #[test]
    fn suggested_prefix_from_call() {
        let call = bash("npm install lodash");
        assert_eq!(
            suggested_command_prefix(&call).as_deref(),
            Some("npm install")
        );
        let edit = ToolCall {
            id: "1".into(),
            name: "edit".into(),
            arguments: json!({ "path": "a.rs", "old_string": "a", "new_string": "b" }),
        };
        assert_eq!(suggested_command_prefix(&edit), None);
    }
}
