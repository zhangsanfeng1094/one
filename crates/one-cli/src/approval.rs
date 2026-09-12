//! Interactive / fail-closed tool permission gate.
//!
//! Combines fine-grained [`one_tools::PermissionRules`] with session memory and
//! an optional UI channel for Ask verdicts.
//!
//! Also enforces workspace [`PathPolicy`] for path tools: read **and write**
//! outside may escalate via interactive Select (`path read:` / `path write:`).
//! Auto / fail-closed / session_auto stay hard-deny (no prompt, no auto-grant).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::tool::ToolCall;
use one_core::tool_gate::{ToolGate, ToolGateDecision};
use one_tools::{
    bash_command, call_fingerprint, call_summary, command_matches_prefix, evaluate_with_mode,
    requires_escalation, suggested_command_prefix, tool_args::path_arg, AccessKind, OsSandbox,
    PathPolicy, PermissionRule, PermissionRules, PermissionVerdict,
};

pub use one_tools::PermissionMode;
use tokio::sync::oneshot;

static REQ_SEQ: AtomicU64 = AtomicU64::new(1);

/// How Ask verdicts are resolved when no session allow exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Block the agent task until the TUI responds (interactive).
    Interactive,
    /// Immediately deny Ask (print / json / rpc without --yes).
    FailClosed,
    /// Treat Ask as Allow (auto_approve / ONE_AUTO_APPROVE / --yes).
    Auto,
}

/// Request shown in the TUI approval overlay.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: u64,
    pub tool: String,
    pub summary: String,
    pub reason: String,
    pub fingerprint: String,
    /// Codex-style command-family prefix when the tool is bash/shell.
    pub suggested_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// Session-wide auto-approve for the rest of this process.
    Always,
    /// Allow this single call.
    Once,
    /// Allow matching fingerprint for the rest of the process.
    Session,
    /// Allow bash/shell commands sharing [`ApprovalRequest::suggested_prefix`].
    ///
    /// Escalation scope is taken from the pending request (sandbox escalate stays
    /// separate from in-sandbox high-risk allows).
    Prefix,
    /// Deny this call; optional feedback is returned to the model.
    Deny { feedback: Option<String> },
}

/// Session allow for a command *family* (Codex "starts with …").
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrefixAllow {
    /// Lowercase tool name (`bash` / `shell` normalized to `bash`).
    tool: String,
    /// When true, only matches `require_escalated` calls.
    escalate_only: bool,
    prefix: String,
}

/// What kind of approval is pending — drives `respond` side effects.
#[derive(Debug, Clone)]
enum PendingKind {
    /// High-risk bash / permission rules Ask (existing Always behavior).
    Standard,
    /// `sandbox_permissions: require_escalated`.
    SandboxEscalate,
    /// bwrap is unavailable in workspace-write. This approval is deliberately
    /// single-use; it permits only this call to fall back to the host shell.
    SandboxUnavailable,
    /// Out-of-workspace path read. Grants applied here; never enable_session_auto.
    PathRead {
        resolved: PathBuf,
        suggested_root: Option<PathBuf>,
        token: String,
    },
    /// Out-of-workspace path write/edit. Grants applied here; never enable_session_auto.
    PathWrite {
        resolved: PathBuf,
        suggested_root: Option<PathBuf>,
        token: String,
    },
}

struct Pending {
    request: ApprovalRequest,
    kind: PendingKind,
    tx: oneshot::Sender<ApprovalChoice>,
}

/// Shared gate installed on the agent.
pub struct PermissionGate {
    rules: Vec<PermissionRule>,
    mode: Mutex<ApprovalMode>,
    permission_mode: Mutex<PermissionMode>,
    /// Set by ApprovalChoice::Always for the rest of the process.
    session_auto: AtomicBool,
    /// TUI (or other) can poll [`Self::poll_request`] and answer.
    /// When false, destructive Ask cannot hang — it is denied.
    interactive: bool,
    session_allows: Mutex<HashSet<String>>,
    session_prefixes: Mutex<Vec<PrefixAllow>>,
    pending: Mutex<Option<Pending>>,
    /// Same PathPolicy shell as tools (shared `dynamic` Arc). Tests may omit.
    path_policy: Option<PathPolicy>,
}

impl PermissionGate {
    pub fn new(rules: PermissionRules, mode: ApprovalMode) -> Arc<Self> {
        Self::new_with_policy(rules, mode, None)
    }

    pub fn new_with_policy(
        rules: PermissionRules,
        mode: ApprovalMode,
        path_policy: Option<PathPolicy>,
    ) -> Arc<Self> {
        // `new` is used by harness / tests without a TUI poller → not interactive.
        let interactive = matches!(mode, ApprovalMode::Interactive);
        Self::new_with_policy_interactive(rules, mode, path_policy, interactive)
    }

    fn new_with_policy_interactive(
        rules: PermissionRules,
        mode: ApprovalMode,
        path_policy: Option<PathPolicy>,
        interactive: bool,
    ) -> Arc<Self> {
        let perm_mode = if matches!(mode, ApprovalMode::Auto) {
            PermissionMode::BypassPermissions
        } else {
            PermissionMode::Default
        };
        Arc::new(Self {
            rules: rules.compiled(),
            mode: Mutex::new(mode),
            permission_mode: Mutex::new(perm_mode),
            session_auto: AtomicBool::new(matches!(mode, ApprovalMode::Auto)),
            interactive,
            session_allows: Mutex::new(HashSet::new()),
            session_prefixes: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            path_policy,
        })
    }

    pub fn with_auto_approve(rules: PermissionRules, auto: bool, interactive: bool) -> Arc<Self> {
        Self::with_auto_approve_and_policy(rules, auto, interactive, None)
    }

    pub fn with_auto_approve_and_policy(
        rules: PermissionRules,
        auto: bool,
        interactive: bool,
        path_policy: Option<PathPolicy>,
    ) -> Arc<Self> {
        let mode = if auto {
            ApprovalMode::Auto
        } else if interactive {
            ApprovalMode::Interactive
        } else {
            ApprovalMode::FailClosed
        };
        Self::new_with_policy_interactive(rules, mode, path_policy, interactive)
    }

    pub fn with_permission_mode_and_policy(
        rules: PermissionRules,
        perm_mode: PermissionMode,
        interactive: bool,
        path_policy: Option<PathPolicy>,
    ) -> Arc<Self> {
        let app_mode = if perm_mode.is_always_approve() {
            ApprovalMode::Auto
        } else if interactive {
            ApprovalMode::Interactive
        } else {
            ApprovalMode::FailClosed
        };
        let gate = Self::new_with_policy_interactive(rules, app_mode, path_policy, interactive);
        *gate.permission_mode.lock().expect("permission mode") = perm_mode;
        gate
    }

    /// Shared policy handle (for wiring asserts / tests).
    pub fn path_policy(&self) -> Option<&PathPolicy> {
        self.path_policy.as_ref()
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.lock().expect("mode lock")
    }

    pub fn permission_mode(&self) -> PermissionMode {
        *self.permission_mode.lock().expect("permission mode lock")
    }

    pub fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), String> {
        if mode.is_always_approve() {
            crate::governance::check_bypass_permissions_allowed()?;
        }
        *self.permission_mode.lock().expect("permission mode lock") = mode;
        if mode.is_always_approve() {
            self.session_auto.store(true, Ordering::Relaxed);
            *self.mode.lock().expect("mode lock") = ApprovalMode::Auto;
        } else {
            self.session_auto.store(false, Ordering::Relaxed);
            if self.interactive {
                *self.mode.lock().expect("mode lock") = ApprovalMode::Interactive;
            } else {
                *self.mode.lock().expect("mode lock") = ApprovalMode::FailClosed;
            }
        }
        Ok(())
    }

    pub fn toggle_always_approve(&self) -> Result<PermissionMode, String> {
        let current = self.permission_mode();
        let next = if current.is_always_approve() || self.session_auto() {
            PermissionMode::Default
        } else {
            PermissionMode::BypassPermissions
        };
        self.set_permission_mode(next)?;
        Ok(next)
    }

    pub fn toggle_auto(&self) -> Result<PermissionMode, String> {
        let current = self.permission_mode();
        let next = if current == PermissionMode::Auto {
            PermissionMode::Default
        } else {
            PermissionMode::Auto
        };
        self.set_permission_mode(next)?;
        Ok(next)
    }

    /// True when Always-approve was chosen (or started in Auto).
    pub fn session_auto(&self) -> bool {
        self.session_auto.load(Ordering::Relaxed)
    }

    /// Enable process-wide auto-approve (permission option / Ctrl+O).
    pub fn enable_session_auto(&self) {
        let _ = self.set_permission_mode(PermissionMode::BypassPermissions);
    }

    /// Non-blocking poll for a pending interactive approval (TUI).
    pub fn poll_request(&self) -> Option<ApprovalRequest> {
        self.pending
            .lock()
            .expect("pending lock")
            .as_ref()
            .map(|p| p.request.clone())
    }

    fn matches_session_prefix(&self, call: &ToolCall) -> bool {
        let Some(cmd) = bash_command(call) else {
            return false;
        };
        let tool_lc = call.name.to_ascii_lowercase();
        let tool = match tool_lc.as_str() {
            "bash" | "shell" => "bash",
            other => other,
        };
        let escalate = requires_escalation(call);
        let prefixes = self.session_prefixes.lock().expect("session prefixes");
        prefixes.iter().any(|pa| {
            pa.tool == tool
                && pa.escalate_only == escalate
                && command_matches_prefix(cmd, &pa.prefix)
        })
    }

    /// Resolve the current pending request (TUI / tests).
    pub fn respond(&self, choice: ApprovalChoice) -> bool {
        let mut g = self.pending.lock().expect("pending lock");
        if let Some(pending) = g.take() {
            match &pending.kind {
                PendingKind::PathRead {
                    resolved,
                    suggested_root,
                    token,
                } => {
                    // Grants applied here; NEVER enable_session_auto for path reads.
                    if let Some(policy) = &self.path_policy {
                        match &choice {
                            ApprovalChoice::Once => {
                                policy.grant_once(token, resolved, AccessKind::Read);
                            }
                            ApprovalChoice::Session | ApprovalChoice::Prefix => {
                                if let Some(root) = suggested_root {
                                    policy.grant_readable_root(root);
                                } else {
                                    policy.grant_read_path(resolved);
                                }
                            }
                            ApprovalChoice::Always => {
                                // Must not flip Auto. Treat as session root or Once.
                                if let Some(root) = suggested_root {
                                    policy.grant_readable_root(root);
                                } else {
                                    policy.grant_read_path(resolved);
                                }
                            }
                            ApprovalChoice::Deny { .. } => {}
                        }
                    }
                }
                PendingKind::PathWrite {
                    resolved,
                    suggested_root,
                    token,
                } => {
                    // Grants applied here; NEVER enable_session_auto for path writes.
                    if let Some(policy) = &self.path_policy {
                        match &choice {
                            ApprovalChoice::Once => {
                                policy.grant_once(token, resolved, AccessKind::Write);
                            }
                            ApprovalChoice::Session | ApprovalChoice::Prefix => {
                                if let Some(root) = suggested_root {
                                    policy.grant_writable_root(root);
                                } else {
                                    policy.grant_write_path(resolved);
                                }
                            }
                            ApprovalChoice::Always => {
                                if let Some(root) = suggested_root {
                                    policy.grant_writable_root(root);
                                } else {
                                    policy.grant_write_path(resolved);
                                }
                            }
                            ApprovalChoice::Deny { .. } => {}
                        }
                    }
                }
                PendingKind::SandboxUnavailable => {}
                PendingKind::Standard | PendingKind::SandboxEscalate => {
                    let escalate = matches!(pending.kind, PendingKind::SandboxEscalate);
                    match &choice {
                        ApprovalChoice::Session => {
                            self.session_allows
                                .lock()
                                .expect("session allows")
                                .insert(pending.request.fingerprint.clone());
                        }
                        ApprovalChoice::Prefix => {
                            if let Some(prefix) = pending.request.suggested_prefix.clone() {
                                let tool_lc = pending.request.tool.to_ascii_lowercase();
                                let tool = match tool_lc.as_str() {
                                    "bash" | "shell" => "bash".to_string(),
                                    other => other.to_string(),
                                };
                                let mut list =
                                    self.session_prefixes.lock().expect("session prefixes");
                                let entry = PrefixAllow {
                                    tool,
                                    escalate_only: escalate,
                                    prefix,
                                };
                                if !list.contains(&entry) {
                                    list.push(entry);
                                }
                            } else {
                                self.session_allows
                                    .lock()
                                    .expect("session allows")
                                    .insert(pending.request.fingerprint.clone());
                            }
                        }
                        ApprovalChoice::Always => {
                            self.enable_session_auto();
                        }
                        _ => {}
                    }
                }
            }
            let _ = pending.tx.send(choice);
            true
        } else {
            false
        }
    }

    /// Abort any waiter (force-quit / turn cancel).
    pub fn cancel_pending(&self) {
        if let Some(pending) = self.pending.lock().expect("pending lock").take() {
            let _ = pending.tx.send(ApprovalChoice::Deny { feedback: None });
        }
    }

    async fn await_approval(
        &self,
        request: ApprovalRequest,
        kind: PendingKind,
    ) -> ToolGateDecision {
        let reason = request.reason.clone();
        let tool = request.tool.clone();
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.pending.lock().expect("pending lock");
            if g.is_some() {
                return ToolGateDecision::Deny {
                    message: "another approval is already pending".into(),
                };
            }
            *g = Some(Pending {
                request: request.clone(),
                kind,
                tx,
            });
        }
        match rx.await {
            Ok(ApprovalChoice::Once)
            | Ok(ApprovalChoice::Session)
            | Ok(ApprovalChoice::Prefix)
            | Ok(ApprovalChoice::Always) => ToolGateDecision::Allow,
            Ok(ApprovalChoice::Deny { feedback }) => {
                let msg = match feedback {
                    Some(fb) if !fb.trim().is_empty() => {
                        format!("user denied tool `{tool}` ({reason}): {fb}")
                    }
                    _ => format!("user denied tool `{tool}` ({reason})"),
                };
                ToolGateDecision::Deny { message: msg }
            }
            Err(_) => ToolGateDecision::Deny {
                message: format!("user denied tool `{tool}` ({reason})"),
            },
        }
    }
}

fn is_path_read_tool(name: &str) -> bool {
    matches!(name, "read" | "grep" | "ls")
}

fn is_path_write_tool(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

/// When Interactive and path Select is allowed (not session_auto / kill-switch).
fn path_prompt_allowed(mode: ApprovalMode, session_auto: bool) -> bool {
    matches!(mode, ApprovalMode::Interactive) && !session_auto && path_read_escalate_env_enabled()
}

/// Kill-switch: `ONE_PATH_READ_ESCALATE=0` disables Select (hard deny).
/// Default: enabled (Select ships with this feature).
fn path_read_escalate_env_enabled() -> bool {
    match std::env::var("ONE_PATH_READ_ESCALATE") {
        Ok(v) => {
            let t = v.trim();
            !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

fn path_from_call(call: &ToolCall) -> std::result::Result<Option<String>, String> {
    match path_arg(&call.arguments)? {
        Some(p) => Ok(Some(p.to_string())),
        None if matches!(call.name.as_str(), "grep" | "ls") => Ok(Some(".".into())),
        None => Ok(None),
    }
}

#[async_trait]
impl ToolGate for PermissionGate {
    async fn check(&self, call: &ToolCall) -> ToolGateDecision {
        // ask_user is itself a HITL tool — never double-prompt via permission UI.
        if call.name == "ask_user" {
            return ToolGateDecision::Allow;
        }

        // Work out the effective mode before checking any cached approval. A
        // cached command family must never shadow a newly-added explicit deny.
        let env_auto = std::env::var("ONE_AUTO_APPROVE")
            .or_else(|_| std::env::var("PI_AUTO_APPROVE"))
            .ok()
            .as_deref()
            == Some("1");
        let mode = self.mode();
        let perm_mode = self.permission_mode();
        let auto = env_auto
            || self.session_auto()
            || matches!(mode, ApprovalMode::Auto)
            || perm_mode.is_always_approve();
        let effective_mode = if auto {
            PermissionMode::BypassPermissions
        } else {
            perm_mode
        };
        let preflight = evaluate_with_mode(call, &self.rules, effective_mode);
        if let PermissionVerdict::Deny { reason } = preflight {
            return ToolGateDecision::Deny { message: reason };
        }

        // Workspace-write without bwrap cannot enforce bash writes. Do not run
        // a best-effort host shell: only an interactive, per-call confirmation
        // may rewrite this exact execution to the explicit escalated path.
        let bwrap_missing = matches!(call.name.as_str(), "bash" | "shell")
            && self
                .path_policy
                .as_ref()
                .is_some_and(|p| !p.is_full_access())
            && !OsSandbox::bwrap_available();
        if bwrap_missing {
            if !self.interactive {
                return ToolGateDecision::Deny {
                    message: "bash sandbox is unavailable (bwrap not found); denied in non-interactive mode".into(),
                };
            }
            let request = ApprovalRequest {
                id: REQ_SEQ.fetch_add(1, Ordering::Relaxed),
                tool: call.name.clone(),
                summary: call_summary(call),
                reason: "bash sandbox is unavailable; run this one command without OS isolation"
                    .into(),
                fingerprint: call_fingerprint(call),
                suggested_prefix: None,
            };
            match self
                .await_approval(request, PendingKind::SandboxUnavailable)
                .await
            {
                ToolGateDecision::Allow => {
                    let mut arguments = call.arguments.clone();
                    if let Some(object) = arguments.as_object_mut() {
                        object.insert(
                            "sandbox_permissions".into(),
                            serde_json::json!("require_escalated"),
                        );
                    }
                    return ToolGateDecision::Rewrite { arguments };
                }
                deny => return deny,
            }
        }

        let fp = call_fingerprint(call);
        if self
            .session_allows
            .lock()
            .expect("session allows")
            .contains(&fp)
        {
            // Still enforce path boundary even for fingerprint-allowed bash? Fingerprints
            // are only inserted for Standard/SandboxEscalate, not path tools. Path re-access
            // is via PathPolicy grants. Fall through only for non-path tools.
            if !is_path_read_tool(&call.name) && !is_path_write_tool(&call.name) {
                return ToolGateDecision::Allow;
            }
        }

        if self.matches_session_prefix(call) {
            return ToolGateDecision::Allow;
        }

        let perm = preflight;
        match perm {
            PermissionVerdict::Deny { reason } => {
                return ToolGateDecision::Deny { message: reason };
            }
            PermissionVerdict::Ask { reason } => {
                // Destructive shapes (git checkout/restore/reset, rm -r, …) always
                // need a real confirmation unless --full-access is explicitly enabled.
                let is_full_access = self
                    .path_policy
                    .as_ref()
                    .is_some_and(|p| p.is_full_access());
                let force =
                    one_tools::sandbox::is_destructive_ask_reason(&reason) && !is_full_access;
                if auto && !force {
                    // Soft high-risk / ask-rule: auto_approve allows.
                } else if force && !self.interactive {
                    // Non-interactive (print/json/harness): refuse rather than hang
                    // waiting for a TUI answer that will never come.
                    return ToolGateDecision::Deny {
                        message: format!(
                            "{reason}. Denied in non-interactive mode — \
                             destructive commands require an interactive confirmation \
                             (cannot be auto-approved with --yes)."
                        ),
                    };
                } else if !auto || force {
                    match mode {
                        ApprovalMode::FailClosed if !self.interactive => {
                            return ToolGateDecision::Deny {
                                message: format!(
                                    "{reason}. Denied in non-interactive mode. \
                                     Re-run with --yes / ONE_AUTO_APPROVE=1, or add an allow rule."
                                ),
                            };
                        }
                        // Interactive TUI (including Auto mode from auto_approve /
                        // Always): surface the Select dock.
                        ApprovalMode::Interactive
                        | ApprovalMode::Auto
                        | ApprovalMode::FailClosed => {
                            let id = REQ_SEQ.fetch_add(1, Ordering::Relaxed);
                            let escalate = requires_escalation(call);
                            let request = ApprovalRequest {
                                id,
                                tool: call.name.clone(),
                                summary: call_summary(call),
                                reason: reason.clone(),
                                fingerprint: fp.clone(),
                                suggested_prefix: suggested_command_prefix(call),
                            };
                            let kind = if escalate {
                                PendingKind::SandboxEscalate
                            } else {
                                PendingKind::Standard
                            };
                            let decision = self.await_approval(request, kind).await;
                            if !matches!(decision, ToolGateDecision::Allow) {
                                return decision;
                            }
                            // After bash approval, still run path phase for path tools.
                        }
                    }
                }
            }
            PermissionVerdict::Allow => {}
        }

        // --- Path boundary phase (after permission rules Allow) ---
        self.check_path_boundary(call).await
    }

    async fn after_tool(
        &self,
        call: &ToolCall,
        _output: &one_core::tool::ToolOutput,
        _is_error: bool,
    ) {
        if let Some(token) = call
            .arguments
            .get("__one_permission_token")
            .and_then(|v| v.as_str())
        {
            if let Some(policy) = &self.path_policy {
                policy.revoke_once(token);
            }
        }
    }
}

impl PermissionGate {
    async fn check_path_boundary(&self, call: &ToolCall) -> ToolGateDecision {
        let Some(policy) = &self.path_policy else {
            // Tests / missing wiring: skip path phase (tools still enforce).
            return ToolGateDecision::Allow;
        };
        if policy.is_full_access() {
            return ToolGateDecision::Allow;
        }

        let name = call.name.as_str();

        if is_path_write_tool(name) {
            let path_str = match path_arg(&call.arguments) {
                Ok(Some(p)) => p,
                Ok(None) => return ToolGateDecision::Allow, // tool will invalid_args
                Err(msg) => return ToolGateDecision::Deny { message: msg },
            };
            return match policy.resolve(path_str, AccessKind::Write) {
                Ok(_) => ToolGateDecision::Allow,
                Err(hard_msg) => {
                    self.prompt_or_deny_path(call, path_str, AccessKind::Write, hard_msg)
                        .await
                }
            };
        }

        if !is_path_read_tool(name) {
            return ToolGateDecision::Allow;
        }

        let path_str = match path_from_call(call) {
            Ok(Some(p)) => p,
            Ok(None) => return ToolGateDecision::Allow,
            Err(msg) => return ToolGateDecision::Deny { message: msg },
        };

        // grep/ls may auto-narrow to overlapping readable roots (e.g. `~/.one`
        // while `~/.one/agent` is readable) without a path-escalation prompt.
        let coverage = if matches!(name, "grep" | "ls") {
            policy.resolve_read(&path_str)
        } else {
            policy
                .resolve(&path_str, AccessKind::Read)
                .map(one_tools::ReadCoverage::Exact)
        };
        match coverage {
            Ok(_) => ToolGateDecision::Allow,
            Err(hard_msg) => {
                self.prompt_or_deny_path(call, &path_str, AccessKind::Read, hard_msg)
                    .await
            }
        }
    }

    async fn prompt_or_deny_path(
        &self,
        call: &ToolCall,
        path_str: &str,
        access: AccessKind,
        hard_msg: String,
    ) -> ToolGateDecision {
        let Some(policy) = &self.path_policy else {
            return ToolGateDecision::Deny { message: hard_msg };
        };
        let mode = self.mode();
        if !path_prompt_allowed(mode, self.session_auto()) {
            return ToolGateDecision::Deny { message: hard_msg };
        }

        let resolved = one_tools::path_policy::resolve_against_cwd(policy.cwd(), path_str);
        let suggested_root = match access {
            AccessKind::Read => policy.suggest_read_root(&resolved),
            AccessKind::Write => policy.suggest_write_root(&resolved),
        };

        let id = REQ_SEQ.fetch_add(1, Ordering::Relaxed);
        let token = format!("path-once:{id}:{}", call.id);
        let root_note = match &suggested_root {
            Some(r) => format!("suggested session root: {}", r.display()),
            None => "suggested session root: (none — this path only)".into(),
        };
        let kind_word = match access {
            AccessKind::Read => "read",
            AccessKind::Write => "write",
        };
        let reason = format!(
            "path {kind_word}: outside workspace\npath: {}\n{root_note}",
            resolved.display()
        );
        let request = ApprovalRequest {
            id,
            tool: call.name.clone(),
            summary: format!("{kind_word} {}", resolved.display()),
            reason,
            fingerprint: call_fingerprint(call),
            suggested_prefix: None,
        };
        let kind = match access {
            AccessKind::Read => PendingKind::PathRead {
                resolved: resolved.clone(),
                suggested_root: suggested_root.clone(),
                token: token.clone(),
            },
            AccessKind::Write => PendingKind::PathWrite {
                resolved: resolved.clone(),
                suggested_root: suggested_root.clone(),
                token: token.clone(),
            },
        };
        match self.await_approval(request, kind).await {
            ToolGateDecision::Allow => {
                let mut arguments = call.arguments.clone();
                if let Some(object) = arguments.as_object_mut() {
                    object.insert("__one_permission_token".into(), serde_json::json!(token));
                }
                ToolGateDecision::Rewrite { arguments }
            }
            deny => deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn fail_closed_denies_high_risk() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, false);
        let decision = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo id" }),
            })
            .await;
        assert!(matches!(decision, ToolGateDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn auto_allows_high_risk() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), true, false);
        let decision = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo id" }),
            })
            .await;
        assert_eq!(decision, ToolGateDecision::Allow);
    }

    #[tokio::test]
    async fn auto_noninteractive_denies_destructive_git() {
        // -y / ONE_AUTO_APPROVE must not wipe a worktree unattended.
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), true, false);
        for cmd in [
            "git checkout -- .",
            "git restore .",
            "git reset --hard",
            "rm -rf ./build",
        ] {
            let decision = gate
                .check(&ToolCall {
                    id: "1".into(),
                    name: "bash".into(),
                    arguments: json!({ "command": cmd }),
                })
                .await;
            match decision {
                ToolGateDecision::Deny { message } => {
                    assert!(
                        message.contains("destructive") || message.contains("always confirm"),
                        "cmd={cmd} message={message}"
                    );
                }
                other => panic!("expected Deny for {cmd}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn destructive_interactive_prompts_despite_auto() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), true, true);
        let g = gate.clone();
        let check = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git checkout -- ." }),
            })
            .await
        });
        // Wait until the prompt is pending.
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let req = gate
            .poll_request()
            .expect("destructive should surface approval dock even with auto_approve");
        assert!(
            one_tools::sandbox::is_destructive_ask_reason(&req.reason),
            "reason={}",
            req.reason
        );
        assert!(gate.respond(ApprovalChoice::Deny { feedback: None }));
        let decision = check.await.expect("join");
        assert!(
            matches!(decision, ToolGateDecision::Deny { .. }),
            "{decision:?}"
        );
    }

    #[tokio::test]
    async fn deny_rule() {
        let mut rules = PermissionRules::default();
        rules.deny.push("Bash(git push *)".into());
        let gate = PermissionGate::new(rules, ApprovalMode::Auto);
        let decision = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git push origin main" }),
            })
            .await;
        assert!(matches!(decision, ToolGateDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn require_escalated_fail_closed() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, false);
        let decision = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "kill 1",
                    "sandbox_permissions": "require_escalated",
                    "justification": "cleanup host process"
                }),
            })
            .await;
        match decision {
            ToolGateDecision::Deny { message } => {
                assert!(
                    message.contains("sandbox escalation"),
                    "expected escalate deny reason, got {message}"
                );
            }
            other => panic!("expected Deny in fail-closed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn require_escalated_interactive_once() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, true);
        let g = gate.clone();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "kill 1",
                    "sandbox_permissions": "require_escalated",
                    "justification": "cleanup"
                }),
            })
            .await
        });
        for _ in 0..50 {
            if let Some(req) = gate.poll_request() {
                assert!(
                    req.reason.starts_with("sandbox escalation:"),
                    "{}",
                    req.reason
                );
                assert!(
                    req.summary.contains("without OS bwrap")
                        || req.summary.contains("outside sandbox"),
                    "{}",
                    req.summary
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Once));
        assert_eq!(handle.await.unwrap(), ToolGateDecision::Allow);
    }

    #[tokio::test]
    async fn always_enables_session_auto() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, true);
        let g = gate.clone();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo id" }),
            })
            .await
        });
        // Wait until pending is set.
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Always));
        let d = handle.await.unwrap();
        assert_eq!(d, ToolGateDecision::Allow);
        assert!(gate.session_auto());
        // Next ask should auto-allow without pending.
        let d2 = gate
            .check(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo whoami" }),
            })
            .await;
        assert_eq!(d2, ToolGateDecision::Allow);
        assert!(gate.poll_request().is_none());
    }

    #[tokio::test]
    async fn prefix_allows_command_family_not_unrelated() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, true);
        let g = gate.clone();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git push origin main" }),
            })
            .await
        });
        for _ in 0..50 {
            if let Some(req) = gate.poll_request() {
                assert_eq!(req.suggested_prefix.as_deref(), Some("git push"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Prefix));
        assert_eq!(handle.await.unwrap(), ToolGateDecision::Allow);

        // Same family — auto allow.
        let d2 = gate
            .check(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git push --force-with-lease origin main" }),
            })
            .await;
        assert_eq!(d2, ToolGateDecision::Allow);
        assert!(gate.poll_request().is_none());

        // Different family — still asks (high-risk sudo).
        let g2 = gate.clone();
        let handle2 = tokio::spawn(async move {
            g2.check(&ToolCall {
                id: "3".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo id" }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.poll_request().is_some());
        assert!(gate.respond(ApprovalChoice::Deny { feedback: None }));
        assert!(matches!(
            handle2.await.unwrap(),
            ToolGateDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn cached_prefix_cannot_shadow_an_explicit_deny_suffix() {
        let gate = PermissionGate::with_auto_approve(
            PermissionRules {
                deny: vec!["Bash(*rm -rf *)".into()],
                ..Default::default()
            },
            false,
            true,
        );
        let g = gate.clone();
        let approved = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git push origin main" }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Prefix));
        assert_eq!(approved.await.unwrap(), ToolGateDecision::Allow);

        let denied = gate
            .check(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({ "command": "git push origin main; rm -rf /tmp/x" }),
            })
            .await;
        assert!(matches!(denied, ToolGateDecision::Deny { .. }));
        assert!(gate.poll_request().is_none());
    }

    #[tokio::test]
    async fn prefix_escalate_does_not_cover_sandboxed() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, true);
        let g = gate.clone();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "sudo id",
                    "sandbox_permissions": "require_escalated",
                    "justification": "need host root"
                }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let req = gate.poll_request().expect("pending");
        // "sudo id" → wrapper+next → "sudo id"
        assert_eq!(req.suggested_prefix.as_deref(), Some("sudo id"));
        assert!(gate.respond(ApprovalChoice::Prefix));
        assert_eq!(handle.await.unwrap(), ToolGateDecision::Allow);

        // Same command family but NOT escalated: still high-risk Ask.
        let g2 = gate.clone();
        let handle2 = tokio::spawn(async move {
            g2.check(&ToolCall {
                id: "2".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo id -u" }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            gate.poll_request().is_some(),
            "sandboxed sudo must not inherit escalate-only prefix allow"
        );
        assert!(gate.respond(ApprovalChoice::Once));
        assert_eq!(handle2.await.unwrap(), ToolGateDecision::Allow);

        // Escalated family still auto-allows.
        let d3 = gate
            .check(&ToolCall {
                id: "3".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "sudo id -a",
                    "sandbox_permissions": "require_escalated",
                    "justification": "again"
                }),
            })
            .await;
        assert_eq!(d3, ToolGateDecision::Allow);
        assert!(gate.poll_request().is_none());
    }

    #[tokio::test]
    async fn deny_with_feedback_message() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, true);
        let g = gate.clone();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "sudo id" }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Deny {
            feedback: Some("use a safer command".into()),
        }));
        match handle.await.unwrap() {
            ToolGateDecision::Deny { message } => {
                assert!(message.contains("use a safer command"), "{message}");
            }
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_tool_always_allowed() {
        let gate = PermissionGate::with_auto_approve(PermissionRules::default(), false, false);
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "ask_user".into(),
                arguments: json!({ "questions": [] }),
            })
            .await;
        assert_eq!(d, ToolGateDecision::Allow);
    }

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "one-gate-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_outside() -> PathBuf {
        let base = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target");
        let dir = base.join(format!(
            "one-gate-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn path_tmp_read_allowed_without_prompt() {
        let ws = temp_outside();
        let tmp_file = std::env::temp_dir().join(format!("one-tmp-img-{}.png", std::process::id()));
        std::fs::write(&tmp_file, b"png").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true, // interactive
            Some(policy),
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": tmp_file.to_str().unwrap() }),
            })
            .await;
        assert_eq!(d, ToolGateDecision::Allow);
        assert!(gate.poll_request().is_none());

        let d = gate
            .check(&ToolCall {
                id: "2".into(),
                name: "write".into(),
                arguments: json!({
                    "path": tmp_file.to_str().unwrap(),
                    "content": "ok",
                }),
            })
            .await;
        assert_eq!(d, ToolGateDecision::Allow, "write /tmp must not prompt");
        assert!(gate.poll_request().is_none());

        let _ = std::fs::remove_file(&tmp_file);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_fail_closed_no_prompt() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            false, // fail-closed
            Some(policy),
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": file.to_str().unwrap() }),
            })
            .await;
        match d {
            ToolGateDecision::Deny { message } => {
                assert!(message.contains("outside workspace"), "{message}");
            }
            other => panic!("expected hard deny, got {other:?}"),
        }
        assert!(gate.poll_request().is_none());
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_auto_mode_hard_deny() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            true, // auto
            false,
            Some(policy),
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": file.to_str().unwrap() }),
            })
            .await;
        assert!(matches!(d, ToolGateDecision::Deny { .. }));
        assert!(gate.poll_request().is_none());
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_once_grant_is_bound_to_the_effective_call() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let tool_policy = policy.clone();
        assert!(Arc::ptr_eq(
            &policy.dynamic_handle(),
            &tool_policy.dynamic_handle()
        ));

        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true,
            Some(policy.clone()),
        );
        assert!(Arc::ptr_eq(
            &policy.dynamic_handle(),
            &gate.path_policy().unwrap().dynamic_handle()
        ));

        let g = gate.clone();
        let path = file.to_str().unwrap().to_string();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": path }),
            })
            .await
        });
        for _ in 0..50 {
            if let Some(req) = gate.poll_request() {
                assert!(req.reason.starts_with("path read:"), "{}", req.reason);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Once));
        let decision = handle.await.unwrap();
        let ToolGateDecision::Rewrite { arguments } = decision else {
            panic!("Once must inject a bound capability");
        };
        let token = arguments["__one_permission_token"].as_str().expect("token");
        tool_policy
            .check(&file, AccessKind::Read)
            .expect_err("ordinary future calls must not inherit Once");
        tool_policy
            .check_with_token(&file, AccessKind::Read, Some(token))
            .expect("only the rewritten call can read");
        tool_policy.revoke_once(token);
        tool_policy
            .check_with_token(&file, AccessKind::Read, Some(token))
            .expect_err("failed, cancelled, or repeated calls cannot reuse Once");
        assert!(!gate.session_auto());

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_always_does_not_enable_session_auto() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true,
            Some(policy.clone()),
        );
        let g = gate.clone();
        let path = file.to_str().unwrap().to_string();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": path }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Force Always as if UI offered it — must not flip session_auto.
        assert!(gate.respond(ApprovalChoice::Always));
        assert!(matches!(
            handle.await.unwrap(),
            ToolGateDecision::Rewrite { .. }
        ));
        assert!(
            !gate.session_auto(),
            "path Always must not enable session_auto"
        );
        assert_eq!(gate.mode(), ApprovalMode::Interactive);
        policy
            .check(&file, AccessKind::Read)
            .expect("Always maps to grant");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_write_outside_hard_deny_fail_closed() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            false, // fail-closed: no prompt
            Some(policy),
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: json!({ "path": file.to_str().unwrap(), "content": "x" }),
            })
            .await;
        match d {
            ToolGateDecision::Deny { message } => {
                assert!(message.contains("outside workspace"), "{message}");
                assert!(message.contains("write"), "{message}");
            }
            other => panic!("expected write deny, got {other:?}"),
        }
        assert!(gate.poll_request().is_none());
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_write_outside_prompts_interactive() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let tool_policy = policy.clone();
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true,
            Some(policy.clone()),
        );
        let g = gate.clone();
        let path = file.to_str().unwrap().to_string();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "path": path,
                    "old_string": "hi",
                    "new_string": "yo",
                }),
            })
            .await
        });
        for _ in 0..50 {
            if let Some(req) = gate.poll_request() {
                assert!(req.reason.starts_with("path write:"), "{}", req.reason);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Once));
        let ToolGateDecision::Rewrite { arguments } = handle.await.unwrap() else {
            panic!("Once write must inject a capability");
        };
        let token = arguments["__one_permission_token"].as_str().expect("token");
        tool_policy
            .check(&file, AccessKind::Write)
            .expect_err("Once write must not persist");
        tool_policy
            .check_with_token(&file, AccessKind::Write, Some(token))
            .expect("effective write call can use its token");
        assert!(!gate.session_auto());

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_write_always_does_not_enable_session_auto() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("x.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true,
            Some(policy.clone()),
        );
        let g = gate.clone();
        let path = file.to_str().unwrap().to_string();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: json!({ "path": path, "content": "x" }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Always));
        assert!(matches!(
            handle.await.unwrap(),
            ToolGateDecision::Rewrite { .. }
        ));
        assert!(
            !gate.session_auto(),
            "path write Always must not enable session_auto"
        );
        policy
            .check(&file, AccessKind::Write)
            .expect("Always maps to write grant");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_write_agent_models_json_allows() {
        let ws = temp_workspace();
        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            false,
            Some(policy),
        );
        let models = format!(
            "{}/.one/agent/models.json",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: json!({ "path": models, "content": "{}" }),
            })
            .await;
        assert_eq!(d, ToolGateDecision::Allow);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_grep_parent_of_readable_root_allows_without_prompt() {
        let ws = temp_workspace();
        let parent = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("target")
            .join(format!(
                "one-gate-narrow-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let allowed = parent.join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::write(allowed.join("a.txt"), "x").unwrap();

        let policy = PathPolicy::workspace(ws.clone()).with_readable_root(allowed.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            false, // fail-closed: would deny if narrowing didn't apply
            Some(policy),
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "grep".into(),
                arguments: json!({
                    "pattern": "x",
                    "path": parent.to_str().unwrap(),
                }),
            })
            .await;
        assert_eq!(d, ToolGateDecision::Allow);
        assert!(gate.poll_request().is_none());
        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_alias_conflict_is_denied() {
        let ws = temp_workspace();
        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true,
            Some(policy),
        );
        let d = gate
            .check(&ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({
                    "path": "/tmp/wrong.rs",
                    "file_path": "src/lib.rs",
                    "old_string": "a",
                    "new_string": "b"
                }),
            })
            .await;
        match d {
            ToolGateDecision::Deny { message } => {
                assert!(message.contains("conflicting path aliases"), "{message}");
            }
            other => panic!("expected conflict deny, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn path_session_root_grant() {
        let ws = temp_workspace();
        let outside = temp_outside();
        let file = outside.join("a.txt");
        let other = outside.join("b.txt");
        std::fs::write(&file, "a").unwrap();
        std::fs::write(&other, "b").unwrap();

        let policy = PathPolicy::workspace(ws.clone());
        let gate = PermissionGate::with_auto_approve_and_policy(
            PermissionRules::default(),
            false,
            true,
            Some(policy.clone()),
        );
        let g = gate.clone();
        let path = file.to_str().unwrap().to_string();
        let handle = tokio::spawn(async move {
            g.check(&ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: json!({ "path": path }),
            })
            .await
        });
        for _ in 0..50 {
            if gate.poll_request().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gate.respond(ApprovalChoice::Session));
        assert!(matches!(
            handle.await.unwrap(),
            ToolGateDecision::Rewrite { .. }
        ));
        // Sibling under session root should pass without another prompt.
        let d2 = gate
            .check(&ToolCall {
                id: "2".into(),
                name: "read".into(),
                arguments: json!({ "path": other.to_str().unwrap() }),
            })
            .await;
        assert_eq!(d2, ToolGateDecision::Allow);
        assert!(gate.poll_request().is_none());

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&ws);
    }
}
