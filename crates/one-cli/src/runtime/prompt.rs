//! User prompt path and context compaction.

use std::sync::Arc;

use one_core::agent::{CompletionRequest, LlmProvider, ThinkingLevel};
use one_core::compaction::{
    attach_compaction_reminder, compact_messages, compacted_live_messages,
    edited_paths_from_messages, estimate_tokens, prefix_fingerprint, prune_old_tool_outputs,
    should_compact_tokens, should_prefire_two_pass, split_for_compaction, split_two_pass,
    summarization_prompt, tokens_for_compaction, two_pass_pass1_prompt, two_pass_pass2_prompt,
    CompactRequest, CompactionConfig, CompactionStateContext, CompactionSuppression,
    PrefireCandidate,
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
        self.inject_mcp_reminder().await;

        let text = self.resources.resolve_prompt(text);
        self.inject_graph_intent_reminder(&text).await;
        self.maybe_compact(provider, CompactRequest::auto()).await?;

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
                    self.maybe_compact(provider, CompactRequest::overflow())
                        .await?;
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

        if result.is_ok() {
            self.note_sampling_success();
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

    /// Clear auto-compact suppression after a successful LLM sampling turn.
    pub fn note_sampling_success(&mut self) {
        if matches!(
            self.compact_suppression,
            CompactionSuppression::StickyUntilSuccess | CompactionSuppression::Turn
        ) {
            self.compact_suppression = CompactionSuppression::None;
        }
    }

    /// Compact with an owned provider so two-pass Pass-1 can prefire in the background.
    pub async fn maybe_compact_with(
        &mut self,
        provider: Arc<dyn LlmProvider>,
        request: CompactRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let prefire = Arc::clone(&provider);
        self.maybe_compact_inner(provider.as_ref(), request, Some(prefire))
            .await
    }

    /// Compact when over threshold, or when `request` forces (`/compact`, overflow).
    ///
    /// Flow (Grok-aligned):
    /// 1. **Prune** every check when enabled (turn-age soft-trim / hard-clear).
    /// 2. **Prefire Pass-1** when `two_pass` and tokens are within the lead band.
    /// 3. **Full compact** at threshold (or force): LLM summary (two-pass if
    ///    enabled / cached NOTE₁), keep_recent tail, session compaction entry,
    ///    live buffer rebuilt the same way resume does.
    pub async fn maybe_compact(
        &mut self,
        provider: &dyn LlmProvider,
        request: CompactRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.maybe_compact_inner(provider, request, None).await
    }

    async fn maybe_compact_inner(
        &mut self,
        provider: &dyn LlmProvider,
        request: CompactRequest,
        prefire_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let settings = crate::settings::load();
        let mut config = settings.compaction_config(self.context_window);
        if request.trigger.ignore_suppression() {
            config.suppression = CompactionSuppression::None;
        } else {
            config.suppression = self.compact_suppression;
        }
        let force = request.force();

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

        // 1) Prune old tool outputs every check (independent of auto-compact).
        if config.prune {
            let n = prune_old_tool_outputs(&mut messages, &config);
            if n > 0 {
                tokens = tokens_for_compaction(&messages, None);
                let mut agent = self.agent.lock().await;
                agent.messages = messages.clone();
                agent.last_prompt_tokens = 0;
            }
        }

        // Manual / overflow always proceed; auto respects `enabled`.
        if !force && !config.enabled {
            return Ok(());
        }

        self.collect_prefire_if_ready().await;
        if !force {
            if let Some(arc) = prefire_provider.as_ref() {
                self.spawn_prefire_pass1(arc.clone(), &messages, &config, tokens);
            }
        }

        let suppressed = !request.trigger.ignore_suppression()
            && config.suppression != CompactionSuppression::None;
        let over_full = force || (!suppressed && should_compact_tokens(tokens, &config));
        if !over_full {
            return Ok(());
        }
        if split_for_compaction(&messages, &config).is_none() {
            return Ok(());
        }

        let matcher = request.trigger.hook_matcher();
        self.extensions.notify_pre_compact(matcher).await;

        let tokens_before = tokens as u64;
        let summary = self
            .summarize_for_compaction(
                provider,
                &messages,
                &config,
                request.instructions.as_deref(),
            )
            .await;
        let (fallback, kept) = compact_messages(&messages, &config);
        let mut summary = summary.unwrap_or(fallback);
        if summary.is_empty() {
            self.compact_suppression = CompactionSuppression::StickyUntilSuccess;
            return Ok(());
        }

        let reminder_ctx = CompactionStateContext {
            cwd: self.cwd.display().to_string(),
            plan_active: self.mode == super::AgentMode::Plan,
            plan_path: self.plan_path.as_ref().map(|p| p.display().to_string()),
            edited_paths: edited_paths_from_messages(&messages),
        };
        summary = attach_compaction_reminder(&summary, &reminder_ctx);

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

        {
            let mut agent = self.agent.lock().await;
            agent.messages = if let Some(session) = &self.session {
                session.build_session_context().messages
            } else {
                compacted_live_messages(&summary, kept)
            };
            agent.last_prompt_tokens = 0;
        }

        self.prefire.cached = None;
        if let Some(handle) = self.prefire.in_flight.take() {
            handle.abort();
        }
        self.compact_suppression = CompactionSuppression::StickyUntilSuccess;
        self.extensions.notify_post_compact(matcher).await;
        Ok(())
    }

    async fn collect_prefire_if_ready(&mut self) {
        let Some(handle) = self.prefire.in_flight.take() else {
            return;
        };
        if !handle.is_finished() {
            self.prefire.in_flight = Some(handle);
            return;
        }
        match handle.await {
            Ok(Some(candidate)) => self.prefire.cached = Some(candidate),
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, "compaction prefire join failed"),
        }
    }

    fn spawn_prefire_pass1(
        &mut self,
        provider: Arc<dyn LlmProvider>,
        messages: &[AgentMessage],
        config: &CompactionConfig,
        tokens: usize,
    ) {
        if !should_prefire_two_pass(tokens, config) {
            return;
        }
        if self.prefire.in_flight.is_some() {
            return;
        }
        let Some((older, _)) = split_for_compaction(messages, config) else {
            return;
        };
        let Some((prefix, _)) = split_two_pass(older) else {
            return;
        };
        let fp = prefix_fingerprint(prefix);
        if self
            .prefire
            .cached
            .as_ref()
            .is_some_and(|c| c.prefix_fingerprint == fp && c.prefix_len == prefix.len())
        {
            return;
        }
        let prefix = prefix.to_vec();
        let prefix_len = prefix.len();
        let prefix_tokens = estimate_tokens(&prefix);
        let prompt = two_pass_pass1_prompt(&prefix);
        let handle = tokio::spawn(async move {
            let text = sample_summary(provider.as_ref(), prompt).await?;
            Some(PrefireCandidate {
                prefix_len,
                prefix_fingerprint: fp,
                prefix_tokens,
                candidate_summary: text,
            })
        });
        self.prefire.in_flight = Some(handle);
        tracing::debug!(
            prefix_len,
            prefix_tokens,
            "compaction prefire Pass-1 started"
        );
    }

    async fn summarize_for_compaction(
        &mut self,
        provider: &dyn LlmProvider,
        messages: &[AgentMessage],
        config: &CompactionConfig,
        instructions: Option<&str>,
    ) -> Option<String> {
        let (older, _) = split_for_compaction(messages, config)?;
        if older.is_empty() {
            return None;
        }

        if config.two_pass {
            if let Some((prefix, suffix)) = split_two_pass(older) {
                // Wait for in-flight Pass-1 if it is the matching prefix.
                if let Some(handle) = self.prefire.in_flight.take() {
                    match handle.await {
                        Ok(Some(candidate)) => self.prefire.cached = Some(candidate),
                        Ok(None) => {}
                        Err(e) => tracing::debug!(error = %e, "compaction prefire join failed"),
                    }
                }
                let fp = prefix_fingerprint(prefix);
                let note1 = if let Some(cached) = self
                    .prefire
                    .cached
                    .as_ref()
                    .filter(|c| c.prefix_fingerprint == fp && c.prefix_len == prefix.len())
                {
                    Some(cached.candidate_summary.clone())
                } else {
                    sample_summary(provider, two_pass_pass1_prompt(prefix)).await
                };
                if let Some(note1) = note1 {
                    if let Some(final_sum) = sample_summary(
                        provider,
                        two_pass_pass2_prompt(&note1, suffix, instructions),
                    )
                    .await
                    {
                        return Some(final_sum);
                    }
                    return Some(note1);
                }
            }
        }

        let prompt = summarization_prompt(older, instructions);
        sample_summary(provider, prompt).await
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

async fn sample_summary(provider: &dyn LlmProvider, prompt: String) -> Option<String> {
    let request = CompletionRequest {
        system_prompt: "You summarize coding-agent conversations for context compaction.".into(),
        messages: vec![AgentMessage::user_text(prompt)],
        tools: Vec::new(),
        server_tools: Vec::new(),
        thinking_level: ThinkingLevel::Off,
    };
    match provider.complete(request).await {
        Ok(response) => {
            let text = one_core::agent::extract_text(&response.content)
                .trim()
                .to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "compaction summarizer failed");
            None
        }
    }
}
