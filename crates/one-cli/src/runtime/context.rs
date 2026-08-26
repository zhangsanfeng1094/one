//! Build a Grok-style `/context` snapshot from the live runtime.

use one_core::{estimate_message_parts, estimate_tokens_str, scale_token_weights, AgentMessage};
use one_resources::skills_catalog_xml;
use one_session::SessionEntry;
use one_tui::{ContextSnapshot, TokenUsageCategory};

use super::AppRuntime;

impl AppRuntime {
    /// Capture how the current context window is being used.
    ///
    /// When the provider reported last-prompt `used`, category numbers are
    /// **weights** (chars/4, images 765) rescaled so they sum to that total:
    /// `display = used * (raw / Σ raw)`. Otherwise raw estimates are shown
    /// and `used` is their sum.
    pub async fn context_snapshot(&self, model: impl Into<String>) -> ContextSnapshot {
        let system_prompt = self.effective_system_prompt();
        let system_raw = estimate_tokens_str(&system_prompt) as u64;

        let agent = self.agent.lock().await;
        let parts = estimate_message_parts(&agent.messages);
        let message_count = agent.messages.len() as u64;
        let turn_count = agent
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User(_)))
            .count() as u64;
        let tool_call_count = agent.messages.iter().map(assistant_tool_calls).sum::<u64>();

        let defs = agent.tool_definitions();
        let tool_json = serde_json::to_string(&defs).unwrap_or_default();
        let tool_definitions_count = defs.len() as u64;
        let tools_raw = estimate_tokens_str(&tool_json) as u64;
        let api_used = agent.last_prompt_tokens;
        drop(agent);

        let skills_raw = {
            let visible = self.resources.model_visible_skills();
            if visible.is_empty() {
                None
            } else {
                let catalog = skills_catalog_xml(&self.resources.skills).unwrap_or_default();
                Some((estimate_tokens_str(&catalog) as u64, visible.len() as u64))
            }
        };

        let mcp_raw = {
            let mcp_snap = self.mcp.status_snapshot();
            let summaries = self.mcp.server_summaries();
            let server_count = mcp_snap.configured.max(summaries.len()) as u64;
            if server_count == 0 {
                None
            } else {
                let text = self.mcp.prompt_announcement().unwrap_or_else(|| {
                    summaries
                        .iter()
                        .map(|s| {
                            format!(
                                "{} ({} {})",
                                s.name,
                                s.tool_count,
                                if s.tool_count == 1 { "tool" } else { "tools" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                Some((estimate_tokens_str(&text) as u64, server_count))
            }
        };

        // Exclusive request pie: system + conversation + reasoning + tool schemas.
        // Skills/MCP overlap system/messages; they ride the same scale factor.
        let pie = [system_raw, parts.messages, parts.reasoning, tools_raw];
        let pie_sum: u64 = pie.iter().copied().sum();

        let (
            used,
            used_estimated,
            system_prompt_tokens,
            message_tokens,
            reasoning_tokens,
            tool_definitions_tokens,
        ) = if api_used > 0 {
            let scaled = scale_token_weights(&pie, api_used);
            (api_used, false, scaled[0], scaled[1], scaled[2], scaled[3])
        } else {
            (
                system_raw
                    .saturating_add(parts.messages)
                    .saturating_add(parts.reasoning),
                pie_sum > 0,
                system_raw,
                parts.messages,
                parts.reasoning,
                tools_raw,
            )
        };

        let mut usage_categories = Vec::new();
        if let Some((raw, count)) = skills_raw {
            usage_categories.push(TokenUsageCategory::skills(
                scale_info_row(raw, pie_sum, used, used_estimated),
                count,
            ));
        }
        if let Some((raw, count)) = mcp_raw {
            usage_categories.push(TokenUsageCategory::mcp_servers(
                scale_info_row(raw, pie_sum, used, used_estimated),
                count,
            ));
        }

        let total = self.context_window as u64;
        let free_tokens = total.saturating_sub(used);
        let usage_pct = if total == 0 {
            0
        } else {
            ((used as f64 / total as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8
        };

        let settings = crate::settings::load();
        let compact = settings.compaction_or_default();
        let auto_compact_enabled = compact.auto.unwrap_or(true);
        let auto_compact_threshold_percent = if let Some(n) = compact.threshold.filter(|n| *n > 0) {
            if total == 0 {
                85
            } else {
                ((n as f64 / total as f64) * 100.0)
                    .round()
                    .clamp(1.0, 100.0) as u8
            }
        } else {
            let r = compact
                .ratio
                .filter(|r| r.is_finite() && *r > 0.0 && *r <= 1.0)
                .unwrap_or(one_core::DEFAULT_COMPACT_RATIO);
            (r * 100.0).round().clamp(1.0, 100.0) as u8
        };

        let compaction_count = self
            .session
            .as_ref()
            .map(|s| {
                s.entries()
                    .iter()
                    .filter(|e| matches!(e, SessionEntry::Compaction { .. }))
                    .count() as u64
            })
            .unwrap_or(0);

        ContextSnapshot {
            used,
            total,
            system_prompt_tokens,
            tool_definitions_count,
            tool_definitions_tokens,
            compaction_count,
            turn_count,
            tool_call_count,
            message_count,
            message_tokens,
            reasoning_tokens,
            free_tokens,
            usage_pct,
            auto_compact_threshold_percent,
            used_estimated,
            auto_compact_enabled,
            model: model.into(),
            usage_categories,
        }
    }
}

/// Skills / MCP overlap the exclusive pie. Apply the same `used / pie` factor
/// so they are in calibrated token units; skip scaling when showing raw estimates.
fn scale_info_row(raw: u64, pie_sum: u64, used: u64, used_estimated: bool) -> u64 {
    if used_estimated || pie_sum == 0 || used == 0 {
        return raw;
    }
    ((raw as u128) * (used as u128) / (pie_sum as u128)) as u64
}

fn assistant_tool_calls(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::Assistant(a) => a
            .content
            .iter()
            .filter(|b| matches!(b, one_core::message::ContentBlock::ToolCall { .. }))
            .count() as u64,
        _ => 0,
    }
}
