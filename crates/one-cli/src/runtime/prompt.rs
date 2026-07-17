//! User prompt path and context compaction.

use one_core::agent::{CompletionRequest, LlmProvider, ThinkingLevel};
use one_core::compaction::{
    compact_messages, should_compact_tokens, split_for_compaction, summarization_prompt,
    tokens_for_compaction, CompactionConfig,
};
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
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(45);
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

        let _ = self
            .extensions
            .emit(&ExtensionEvent::UserPromptSubmit {
                text: text.clone(),
            })
            .await;

        let before = {
            let agent = self.agent.lock().await;
            agent.messages.len()
        };

        let output = match {
            let mut agent = self.agent.lock().await;
            agent.prompt(provider, &text).await
        } {
            Ok(out) => out,
            Err(err) if is_overflow_err(&err) => {
                // Force compact then retry once.
                drop(err);
                self.maybe_compact(provider, true).await?;
                let mut agent = self.agent.lock().await;
                // Drop the user message that was pushed before failure if present twice risk:
                // agent.prompt already pushed user text — on error messages may include it.
                // Retry by calling run() if last message is the user text, else re-prompt.
                if agent
                    .messages
                    .last()
                    .map(|m| matches!(m, AgentMessage::User(_)))
                    .unwrap_or(false)
                {
                    agent.run(provider).await?
                } else {
                    agent.prompt(provider, &text).await?
                }
            }
            Err(err) => return Err(err.into()),
        };

        if let Some(session) = &mut self.session {
            let messages = self.agent.lock().await.messages[before..].to_vec();
            for message in messages {
                session.append_message(message).await?;
            }
        }

        self.persist_extension_state().await?;
        // SessionStart/SessionEnd also fire from AgentHooks inside the loop.
        Ok(output)
    }

    /// Compact when over threshold, or when `force` (e.g. context overflow recovery).
    ///
    /// Threshold is ~70% of [`Self::context_window`] when known; otherwise 80k.
    /// Token pressure prefers last provider-reported prompt size over char/4 estimate.
    pub async fn maybe_compact(
        &mut self,
        provider: &dyn LlmProvider,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = CompactionConfig::from_context_window(self.context_window);
        let (messages, last_prompt) = {
            let agent = self.agent.lock().await;
            (agent.messages.clone(), agent.last_prompt_tokens)
        };
        let observed = if last_prompt > 0 {
            Some(last_prompt)
        } else {
            None
        };
        let tokens = tokens_for_compaction(&messages, observed);

        if !force && !should_compact_tokens(tokens, &config) {
            return Ok(());
        }
        if split_for_compaction(&messages, &config).is_none() {
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

        let first_kept = self
            .session
            .as_ref()
            .and_then(|s| s.get_leaf_id().map(|s| s.to_string()))
            .unwrap_or_else(|| "root".into());

        if let Some(session) = &mut self.session {
            session
                .append_compaction(&summary, first_kept, tokens_before)
                .await?;
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
