//! User prompt path and context compaction.

use one_core::agent::{CompletionRequest, LlmProvider, ThinkingLevel};
use one_core::compaction::{
    compact_messages, prune_old_tool_outputs, should_compact_tokens, should_prefire_prune,
    split_for_compaction, summarization_prompt, tokens_for_compaction, CompactionConfig,
};
use one_core::error::OneError;
use one_core::message::AgentMessage;
use one_ext::ExtensionEvent;

use super::helpers::is_overflow_err;
use super::AppRuntime;

impl AppRuntime {
    pub async fn prompt(
        &mut self,
        provider: &dyn LlmProvider,
        text: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // MCP async load (Grok-style):
        // - If still loading and no tools yet, wait up to 45s for the *first*
        //   server so `-p` / cold start don't race empty tool lists.
        // - Once any tools exist, proceed immediately (more servers trickle in
        //   and attach on subsequent turns via generation sync).
        if self.mcp.is_loading() && self.mcp.tool_count() == 0 {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(45);
            while self.mcp.is_loading()
                && self.mcp.tool_count() == 0
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        self.sync_mcp_tools().await?;

        let text = self.resources.resolve_prompt(text);
        self.maybe_compact(provider, false).await?;

        // M3: memory read/grep budget is per user turn.
        self.memory_lookups.reset_turn();

        let _ = self
            .extensions
            .emit(&ExtensionEvent::UserPromptSubmit { text: text.clone() })
            .await;

        let mut before = {
            let agent = self.agent.lock().await;
            agent.messages.len()
        };
        self.begin_run_meta();
        let usage_before = {
            let agent = self.agent.lock().await;
            agent.token_usage
        };

        let result: Result<String, Box<dyn std::error::Error>> = async {
            match {
                let mut agent = self.agent.lock().await;
                agent.prompt(provider, &text).await
            } {
                Ok(out) => Ok(out),
                Err(err) if is_overflow_err(&err) => {
                    drop(err);
                    self.maybe_compact(provider, true).await?;
                    // Buffer shrank. Re-base so we (1) don't panic on `[before..]` and
                    // (2) still persist the in-flight user turn (never written yet) without
                    // re-appending already-on-disk kept history.
                    before = {
                        let agent = self.agent.lock().await;
                        agent
                            .messages
                            .iter()
                            .rposition(|m| matches!(m, AgentMessage::User(_)))
                            .unwrap_or(agent.messages.len())
                    };
                    let mut agent = self.agent.lock().await;
                    if agent
                        .messages
                        .last()
                        .map(|m| matches!(m, AgentMessage::User(_)))
                        .unwrap_or(false)
                    {
                        Ok(agent.run(provider).await?)
                    } else {
                        Ok(agent.prompt(provider, &text).await?)
                    }
                }
                Err(err) => Err(err.into()),
            }
        }
        .await;

        // Always persist new messages (including failed / partial turns).
        if let Err(e) = self.append_session_delta(before).await {
            tracing::warn!(error = %e, "failed to append session messages after prompt");
        }
        if let Err(e) = self.persist_extension_state().await {
            tracing::warn!(error = %e, "failed to persist extension state after prompt");
        }
        // Usage / tool audit / summary sidecar (never enters LLM context).
        self.persist_run_meta(usage_before, before).await;

        // Grok Build–style: durable turn failure without polluting chat history.
        if let Err(ref err) = result {
            if let Some(one) = err.downcast_ref::<OneError>() {
                self.persist_run_error_one(one).await;
            } else {
                self.persist_run_error("unknown", &err.to_string(), "error")
                    .await;
            }
        }

        result
    }

    /// Append agent messages from `before..` onto the session (best-effort bounds).
    pub async fn append_session_delta(
        &mut self,
        before: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(session) = &mut self.session else {
            return Ok(());
        };
        let agent = self.agent.lock().await;
        let start = before.min(agent.messages.len());
        let messages = agent.messages[start..].to_vec();
        drop(agent);
        for message in messages {
            session.append_message(message).await?;
        }
        Ok(())
    }

    /// Compact when over threshold, or when `force` (e.g. context overflow recovery / `/compact`).
    ///
    /// Strategy from `settings.compaction` (auto / ratio|threshold / keep_recent;
    /// prune default **on** + prefire at ~85% of threshold).
    /// Token pressure prefers last provider-reported prompt size over char/4 estimate.
    ///
    /// Flow:
    /// 1. **Prefire** (tokens ≥ prefire_ratio × threshold, prune on): clear old tool
    ///    bodies outside keep_recent. If that alone drops under the full threshold,
    ///    stop — no LLM summary yet.
    /// 2. **Full compact** (tokens ≥ threshold, or force): prune again if needed, then
    ///    LLM/extractive summary of older messages; keep_recent tail kept verbatim.
    pub async fn maybe_compact(
        &mut self,
        provider: &dyn LlmProvider,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let settings = crate::settings::load();
        let config = settings.compaction_config(self.context_window);

        // Manual force (/compact, API overflow) always proceeds; auto respects `enabled`.
        if !force && !config.enabled {
            return Ok(());
        }

        let (mut messages, last_prompt) = {
            let agent = self.agent.lock().await;
            (agent.messages.clone(), agent.last_prompt_tokens)
        };
        let observed = if last_prompt > 0 {
            Some(last_prompt)
        } else {
            None
        };
        let mut tokens = tokens_for_compaction(&messages, observed);

        let over_full = force || should_compact_tokens(tokens, &config);
        let over_prefire = !force && should_prefire_prune(tokens, &config);
        if !over_full && !over_prefire {
            return Ok(());
        }

        // 1) Prune old tool outputs (prefire and full path). Cheap; may be enough alone.
        let mut did_prune = false;
        if config.prune {
            let n = prune_old_tool_outputs(&mut messages, &config);
            if n > 0 {
                did_prune = true;
                // After prune, re-estimate (provider last_prompt is stale for size).
                tokens = tokens_for_compaction(&messages, None);
                let mut agent = self.agent.lock().await;
                agent.messages = messages.clone();
                // Stale API size no longer matches pruned buffer.
                agent.last_prompt_tokens = 0;
            }
        }

        // Prefire / prune-only: if under full threshold, skip LLM summary unless forced.
        if !force && !should_compact_tokens(tokens, &config) {
            if did_prune || over_prefire {
                tracing::debug!(
                    tokens,
                    threshold = config.token_threshold,
                    "compaction prefire/prune-only; skipping LLM summary"
                );
            }
            return Ok(());
        }
        if split_for_compaction(&messages, &config).is_none() {
            // Nothing left to summarize (too few messages); prune may still have applied.
            return Ok(());
        }

        let tokens_before = tokens as u64;
        let summary = self
            .summarize_for_compaction(provider, &messages, &config)
            .await;
        let (fallback, kept) = compact_messages(&messages, &config);
        let summary = summary.unwrap_or(fallback);
        if summary.is_empty() {
            return Ok(());
        }

        // first_kept = oldest kept context message entry (not the current leaf).
        let first_kept = self
            .session
            .as_ref()
            .map(|s| s.first_kept_entry_id_for_tail(kept.len()))
            .unwrap_or_else(|| "root".into());

        if let Some(session) = &mut self.session {
            session
                .append_compaction(&summary, first_kept, tokens_before)
                .await?;
        }

        // M5: optional L4 archive under memory/sessions/ (not injected into L2).
        // Whole package must be on (feature `memory`).
        if self.applied_features.memory_enabled()
            && settings.memory_or_default().archive_compaction_enabled()
        {
            let sid = self
                .session
                .as_ref()
                .map(|s| s.header().id.clone())
                .unwrap_or_else(|| "no-session".into());
            match one_resources::archive_session_summary(
                &self.resources.agent_dir,
                &self.cwd,
                &sid,
                &summary,
            ) {
                Ok(path) => tracing::info!(
                    path = %path.display(),
                    "archived compaction summary to memory L4"
                ),
                Err(e) => tracing::warn!(error = %e, "memory L4 archive failed"),
            }
        }

        let mut agent = self.agent.lock().await;
        agent.messages = kept;
        // After compact the buffer is much smaller; clear stale API size so the
        // next turn re-estimates until a new completion reports usage.
        agent.last_prompt_tokens = 0;
        agent.messages.insert(
            0,
            AgentMessage::assistant_text(provider.name(), provider.model(), &summary),
        );
        Ok(())
    }

    async fn summarize_for_compaction(
        &self,
        provider: &dyn LlmProvider,
        messages: &[AgentMessage],
        config: &CompactionConfig,
    ) -> Option<String> {
        let (older, _) = split_for_compaction(messages, config)?;
        if older.is_empty() {
            return None;
        }
        let prompt = summarization_prompt(older, None);
        let request = CompletionRequest {
            system_prompt: "You summarize coding-agent conversations for context compaction."
                .into(),
            messages: vec![AgentMessage::user_text(prompt)],
            tools: Vec::new(),
            server_tools: Vec::new(),
            thinking_level: ThinkingLevel::Off,
        };
        match provider.complete(request).await {
            Ok(response) => {
                let text = one_core::agent::extract_text(&response.content);
                let text = text.trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(format!(
                        "Earlier conversation summary ({} messages):\n{}",
                        older.len(),
                        text
                    ))
                }
            }
            Err(_) => None,
        }
    }

    pub async fn persist_extension_state(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let states = self.extensions.custom_states();
        if let Some(session) = &mut self.session {
            for (custom_type, data) in states {
                session.append_custom(custom_type, data).await?;
            }
        }
        Ok(())
    }
}
