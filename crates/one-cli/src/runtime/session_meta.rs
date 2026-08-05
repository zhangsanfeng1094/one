//! Persist session durability metadata (usage, tool audit, prompt snapshot, summary).
//!
//! All writes are best-effort and never enter the LLM context (`SessionEntry::Custom`
//! + sidecar). Failures are logged and ignored so the user turn still succeeds.

use std::time::{SystemTime, UNIX_EPOCH};

use one_core::error::OneError;
use one_core::message::AgentMessage;
use one_core::TokenUsage;
use one_session::{
    prompt_hash, ErrorMeta, PromptSnapshotMeta, ToolAuditItem, ToolAuditMeta, UsageMeta,
};

use super::AppRuntime;

impl AppRuntime {
    /// Clear the in-run tool audit buffer (call before agent run).
    pub fn begin_run_meta(&mut self) {
        self.tool_audit.clear();
        self.tool_starts.clear();
    }

    /// Record tool start (memory only; never blocks on disk).
    pub fn note_tool_start(&mut self, tool_call_id: &str, name: &str) {
        let now = now_ms();
        self.tool_starts
            .insert(tool_call_id.to_string(), (name.to_string(), now));
    }

    /// Record tool end (memory only).
    pub fn note_tool_end(&mut self, tool_call_id: &str, name: &str, is_error: bool) {
        let ended = now_ms();
        let (resolved_name, started) = self
            .tool_starts
            .remove(tool_call_id)
            .unwrap_or_else(|| (name.to_string(), ended));
        let duration_ms = ended.saturating_sub(started);
        self.tool_audit.push(ToolAuditItem {
            tool_call_id: tool_call_id.to_string(),
            name: if resolved_name.is_empty() {
                name.to_string()
            } else {
                resolved_name
            },
            duration_ms: Some(duration_ms),
            is_error,
            gate: None,
            started_at_ms: Some(started),
            ended_at_ms: Some(ended),
        });
    }

    /// After messages are appended: write `one.usage`, `one.tool_audit`, and summary.
    pub async fn persist_run_meta(
        &mut self,
        usage_before: TokenUsage,
        messages_before: usize,
    ) {
        let (total, context_size, provider, model, new_msgs) = {
            let agent = self.agent.lock().await;
            let total = agent.token_usage;
            let context_size = agent.last_prompt_tokens;
            let provider = agent
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    AgentMessage::Assistant(a) => Some(a.provider.clone()),
                    _ => None,
                });
            let model = agent
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    AgentMessage::Assistant(a) => Some(a.model.clone()),
                    _ => None,
                });
            let start = messages_before.min(agent.messages.len());
            let new_msgs = agent.messages[start..].to_vec();
            (total, context_size, provider, model, new_msgs)
        };

        let delta = total.saturating_sub(&usage_before);
        let prompt_index = self.prompt_index;
        self.prompt_index = self.prompt_index.saturating_add(1);

        // If no live tool audit (print mode), synthesize from new tool results.
        if self.tool_audit.is_empty() {
            for m in &new_msgs {
                if let AgentMessage::ToolResult(tr) = m {
                    self.tool_audit.push(ToolAuditItem {
                        tool_call_id: tr.tool_call_id.clone(),
                        name: tr.tool_name.clone(),
                        duration_ms: None,
                        is_error: tr.is_error,
                        gate: None,
                        started_at_ms: None,
                        ended_at_ms: None,
                    });
                }
            }
        }

        let tools = std::mem::take(&mut self.tool_audit);
        self.tool_starts.clear();

        let Some(session) = &mut self.session else {
            return;
        };

        if !delta.is_zero() || !total.is_zero() {
            let meta = UsageMeta::new(
                delta,
                total,
                context_size,
                provider,
                model,
                Some(prompt_index),
            );
            if let Err(e) = session.append_usage(&meta).await {
                tracing::warn!(error = %e, "failed to append one.usage");
            }
        }

        if !tools.is_empty() {
            let audit = ToolAuditMeta::new(Some(prompt_index), tools);
            if let Err(e) = session.append_tool_audit(&audit).await {
                tracing::warn!(error = %e, "failed to append one.tool_audit");
            }
        }

        if let Err(e) = session.write_summary() {
            tracing::warn!(error = %e, "failed to write session summary sidecar");
        }
    }

    /// Persist a terminal run failure as `one.error` (Grok Build–style).
    ///
    /// Call **after** [`Self::persist_run_meta`] so `prompt_index` matches the
    /// usage/audit row for the finished run (`prompt_index` was already advanced).
    /// Does **not** enter LLM context (`SessionEntry::Custom`).
    pub async fn persist_run_error(
        &mut self,
        kind: &str,
        message: &str,
        stop_reason: &str,
    ) {
        let (provider, model) = {
            let agent = self.agent.lock().await;
            let provider = agent.messages.iter().rev().find_map(|m| match m {
                AgentMessage::Assistant(a) => Some(a.provider.clone()),
                _ => None,
            });
            let model = agent.messages.iter().rev().find_map(|m| match m {
                AgentMessage::Assistant(a) => Some(a.model.clone()),
                _ => None,
            });
            (provider, model)
        };
        // Finished run index = last value handed out by persist_run_meta.
        let prompt_index = self.prompt_index.saturating_sub(1);
        let meta = ErrorMeta::new(
            kind,
            message,
            stop_reason,
            Some(prompt_index),
            provider,
            model,
        );
        let Some(session) = &mut self.session else {
            return;
        };
        if let Err(e) = session.append_error(&meta).await {
            tracing::warn!(error = %e, "failed to append one.error");
        }
        if let Err(e) = session.write_summary() {
            tracing::warn!(error = %e, "failed to write session summary after one.error");
        }
    }

    /// Classify and persist [`OneError`] after a failed prompt run.
    pub async fn persist_run_error_one(&mut self, err: &OneError) {
        self.persist_run_error(err.error_kind(), &err.to_string(), err.stop_reason_label())
            .await;
    }

    /// Snapshot the live system prompt when it changes (create / reload / new).
    pub async fn maybe_persist_prompt_snapshot(&mut self, source: &str) {
        let (text, provider, model) = {
            let agent = self.agent.lock().await;
            let text = agent.config.system_prompt.clone();
            // Best-effort last known model from session context / messages.
            let model = agent
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    AgentMessage::Assistant(a) => Some(a.model.clone()),
                    _ => None,
                });
            let provider = agent
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    AgentMessage::Assistant(a) => Some(a.provider.clone()),
                    _ => None,
                });
            (text, provider, model)
        };
        if text.is_empty() {
            return;
        }
        let hash = prompt_hash(&text);
        let Some(session) = &mut self.session else {
            return;
        };
        if session.latest_prompt_hash().as_deref() == Some(hash.as_str()) {
            return;
        }
        let meta = PromptSnapshotMeta {
            schema: one_session::meta::META_SCHEMA,
            hash,
            byte_len: text.len(),
            text: Some(text),
            path: None,
            source: Some(source.into()),
            cwd: Some(self.cwd.display().to_string()),
            provider,
            model,
        };
        if let Err(e) = session.append_prompt_snapshot(meta).await {
            tracing::warn!(error = %e, "failed to append one.prompt_snapshot");
        }
        if let Err(e) = session.write_summary() {
            tracing::warn!(error = %e, "failed to write session summary after prompt snapshot");
        }
    }

    /// Best-effort summary rewrite (rename / rewind / model change).
    pub fn refresh_session_summary(&self) {
        if let Some(session) = &self.session {
            if let Err(e) = session.write_summary() {
                tracing::warn!(error = %e, "failed to refresh session summary");
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
