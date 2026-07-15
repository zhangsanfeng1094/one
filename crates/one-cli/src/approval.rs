//! Interactive / fail-closed tool permission gate.
//!
//! Combines fine-grained [`one_tools::PermissionRules`] with session memory and
//! an optional UI channel for Ask verdicts.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::tool::ToolCall;
use one_core::tool_gate::{ToolGate, ToolGateDecision};
use one_tools::{
    call_fingerprint, call_summary, evaluate_permissions, PermissionRule, PermissionRules,
    PermissionVerdict,
};
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// Session-wide auto-approve for the rest of this process.
    Always,
    /// Allow this single call.
    Once,
    /// Allow matching fingerprint for the rest of the process.
    Session,
    /// Deny this call; optional feedback is returned to the model.
    Deny { feedback: Option<String> },
}

struct Pending {
    request: ApprovalRequest,
    tx: oneshot::Sender<ApprovalChoice>,
}

/// Shared gate installed on the agent.
pub struct PermissionGate {
    rules: Vec<PermissionRule>,
    mode: Mutex<ApprovalMode>,
    /// Set by ApprovalChoice::Always for the rest of the process.
    session_auto: AtomicBool,
    session_allows: Mutex<HashSet<String>>,
    pending: Mutex<Option<Pending>>,
}

impl PermissionGate {
    pub fn new(rules: PermissionRules, mode: ApprovalMode) -> Arc<Self> {
        Arc::new(Self {
            rules: rules.compiled(),
            mode: Mutex::new(mode),
            session_auto: AtomicBool::new(matches!(mode, ApprovalMode::Auto)),
            session_allows: Mutex::new(HashSet::new()),
            pending: Mutex::new(None),
        })
    }

    pub fn with_auto_approve(rules: PermissionRules, auto: bool, interactive: bool) -> Arc<Self> {
        let mode = if auto {
            ApprovalMode::Auto
        } else if interactive {
            ApprovalMode::Interactive
        } else {
            ApprovalMode::FailClosed
        };
        Self::new(rules, mode)
    }

    pub fn mode(&self) -> ApprovalMode {
        *self.mode.lock().expect("mode lock")
    }

    /// True when Always-approve was chosen (or started in Auto).
    pub fn session_auto(&self) -> bool {
        self.session_auto.load(Ordering::Relaxed)
    }

    /// Enable process-wide auto-approve (permission option 1 / Ctrl+O).
    pub fn enable_session_auto(&self) {
        self.session_auto.store(true, Ordering::Relaxed);
        *self.mode.lock().expect("mode lock") = ApprovalMode::Auto;
    }

    /// Non-blocking poll for a pending interactive approval (TUI).
    pub fn poll_request(&self) -> Option<ApprovalRequest> {
        self.pending
            .lock()
            .expect("pending lock")
            .as_ref()
            .map(|p| p.request.clone())
    }

    /// Resolve the current pending request (TUI / tests).
    pub fn respond(&self, choice: ApprovalChoice) -> bool {
        let mut g = self.pending.lock().expect("pending lock");
        if let Some(pending) = g.take() {
            match &choice {
                ApprovalChoice::Session => {
                    self.session_allows
                        .lock()
                        .expect("session allows")
                        .insert(pending.request.fingerprint.clone());
                }
                ApprovalChoice::Always => {
                    self.enable_session_auto();
                }
                _ => {}
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
}

#[async_trait]
impl ToolGate for PermissionGate {
    async fn check(&self, call: &ToolCall) -> ToolGateDecision {
        // ask_user is itself a HITL tool — never double-prompt via permission UI.
        if call.name == "ask_user" {
            return ToolGateDecision::Allow;
        }

        let fp = call_fingerprint(call);
        if self
            .session_allows
            .lock()
            .expect("session allows")
            .contains(&fp)
        {
            return ToolGateDecision::Allow;
        }

        // Env override always wins for automation.
        let env_auto = std::env::var("ONE_AUTO_APPROVE")
            .or_else(|_| std::env::var("PI_AUTO_APPROVE"))
            .ok()
            .as_deref()
            == Some("1");

        let mode = self.mode();
        let auto = env_auto || self.session_auto() || matches!(mode, ApprovalMode::Auto);
        match evaluate_permissions(call, &self.rules, auto) {
            PermissionVerdict::Allow => ToolGateDecision::Allow,
            PermissionVerdict::Deny { reason } => ToolGateDecision::Deny { message: reason },
            PermissionVerdict::Ask { reason } => {
                if auto {
                    return ToolGateDecision::Allow;
                }
                match mode {
                    ApprovalMode::Auto => ToolGateDecision::Allow,
                    ApprovalMode::FailClosed => ToolGateDecision::Deny {
                        message: format!(
                            "{reason}. Denied in non-interactive mode. \
                             Re-run with --yes / ONE_AUTO_APPROVE=1, or add an allow rule."
                        ),
                    },
                    ApprovalMode::Interactive => {
                        let id = REQ_SEQ.fetch_add(1, Ordering::Relaxed);
                        let request = ApprovalRequest {
                            id,
                            tool: call.name.clone(),
                            summary: call_summary(call),
                            reason: reason.clone(),
                            fingerprint: fp.clone(),
                        };
                        let (tx, rx) = oneshot::channel();
                        {
                            let mut g = self.pending.lock().expect("pending lock");
                            // If something is already pending, deny this one (shouldn't happen serially).
                            if g.is_some() {
                                return ToolGateDecision::Deny {
                                    message: "another approval is already pending".into(),
                                };
                            }
                            *g = Some(Pending {
                                request: request.clone(),
                                tx,
                            });
                        }
                        match rx.await {
                            Ok(ApprovalChoice::Once)
                            | Ok(ApprovalChoice::Session)
                            | Ok(ApprovalChoice::Always) => ToolGateDecision::Allow,
                            Ok(ApprovalChoice::Deny { feedback }) => {
                                let msg = match feedback {
                                    Some(fb) if !fb.trim().is_empty() => format!(
                                        "user denied tool `{}` ({reason}): {fb}",
                                        call.name
                                    ),
                                    _ => format!("user denied tool `{}` ({reason})", call.name),
                                };
                                ToolGateDecision::Deny { message: msg }
                            }
                            Err(_) => ToolGateDecision::Deny {
                                message: format!(
                                    "user denied tool `{}` ({reason})",
                                    call.name
                                ),
                            },
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
