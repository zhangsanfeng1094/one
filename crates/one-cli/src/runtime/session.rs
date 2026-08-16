//! Session open / new / naming / thinking metadata.

use one_core::agent::ThinkingLevel;
use one_session::{SessionInfo, SessionManager};

use super::helpers::load_extension_state;
use super::{AgentMode, AppRuntime};

impl AppRuntime {
    pub async fn new_session(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Conversation switch only when replacing an existing session.
        // Cold start already emitted SessionStart from extension load_all.
        let switching = self.session.is_some();
        if switching {
            // Background bash/jobs are session-owned — do not leak across /new.
            self.shutdown_owned_tasks();
            self.extensions.notify_session_end().await;
        }
        // New conversation only — MCP connection pool is process-scoped (Grok-style).
        self.session = Some(SessionManager::create(&self.cwd).await?);
        {
            let mut agent = self.agent.lock().await;
            agent.messages.clear();
            if let Some(s) = &self.session {
                agent.set_trace_session_id(Some(s.header().id.clone()));
            }
        }
        // Apply pending feature flags (context-affecting) now that history is clear.
        self.apply_features_from_settings().await?;
        // New session: refresh env + memory L2 (session-frozen snapshots).
        // apply_features_from_settings already refreshed memory; re-apply in case
        // settings.memory.* changed without feature fingerprint change.
        let settings = crate::settings::load();
        let mem_opts = super::features::effective_memory_options(&self.applied_features, &settings);
        self.refresh_context_snapshots(&mem_opts).await;
        {
            let prompt = self.effective_system_prompt();
            let mut agent = self.agent.lock().await;
            agent.config.system_prompt = prompt;
        }
        // Ensure any MCP servers that finished loading attach to this clean slate.
        self.sync_mcp_tools().await?;
        if switching {
            self.extensions.notify_session_start().await;
        }
        // New conversation gets a fresh full MCP reminder on its first prompt.
        self.mcp_reminder_state.reset();
        self.prompt_index = 0;
        self.tool_audit.clear();
        self.tool_starts.clear();
        self.maybe_persist_prompt_snapshot("new").await;
        self.refresh_session_summary();
        Ok(())
    }

    pub async fn open_session_path(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let switching = self.session.is_some();
        if switching {
            // Background bash/jobs are session-owned — do not leak across /resume.
            self.shutdown_owned_tasks();
            self.extensions.notify_session_end().await;
        }
        let session = SessionManager::open(path).await?;
        {
            let mut agent = self.agent.lock().await;
            agent.messages.clear();
            session.load_messages_into(&mut agent.messages);
            if let Some(level) = session.build_session_context().thinking_level {
                if let Some(tl) = ThinkingLevel::parse(&level) {
                    agent.config.thinking_level = tl;
                }
            }
            agent.set_trace_session_id(Some(session.header().id.clone()));
            // Restore cumulative usage for UI after resume.
            if let Some(total) = session.latest_usage_total() {
                if agent.token_usage.is_zero() {
                    agent.token_usage = total;
                }
            }
        }
        load_extension_state(self.extensions.as_ref(), &session);
        self.mcp_reminder_state.reset();
        self.session = Some(session);
        if switching {
            self.extensions.notify_session_start().await;
        }

        // Restore plan path / mode from session custom entries.
        if !self.read_only {
            if let Some(p) = self.restore_plan_path_from_session() {
                self.plan_path = Some(p);
            }
            match self.restore_mode_from_session().unwrap_or(AgentMode::Act) {
                AgentMode::Plan => {
                    let _ = self.enter_plan_mode().await;
                }
                AgentMode::Act => {
                    if self.mode == AgentMode::Plan {
                        let _ = self.leave_plan_mode().await;
                    }
                }
            }
        }
        self.tool_audit.clear();
        self.tool_starts.clear();
        // prompt_index continues from latest usage row when present.
        if let Some(session) = &self.session {
            if let Some(meta) = session.latest_usage_meta() {
                if let Some(idx) = meta.prompt_index {
                    self.prompt_index = idx.saturating_add(1);
                }
            }
        }
        self.maybe_persist_prompt_snapshot("resume").await;
        self.refresh_session_summary();
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        Ok(SessionManager::list(&self.cwd).await?)
    }

    pub async fn set_session_name(&mut self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(session) = &mut self.session {
            session.append_session_info(name).await?;
            let _ = session.write_summary();
        }
        Ok(())
    }

    pub async fn set_thinking_level(
        &mut self,
        level: ThinkingLevel,
    ) -> Result<(), Box<dyn std::error::Error>> {
        {
            let mut agent = self.agent.lock().await;
            agent.config.thinking_level = level;
        }
        if let Some(session) = &mut self.session {
            session.append_thinking_level_change(level.as_str()).await?;
            let _ = session.write_summary();
        }
        Ok(())
    }

    pub async fn thinking_level(&self) -> ThinkingLevel {
        self.agent.lock().await.config.thinking_level
    }

    /// Live-update empty-completion re-sample budget (from `/settings`).
    pub async fn set_empty_response_retries(&self, retries: usize) {
        self.agent.lock().await.config.empty_response_retries = retries;
    }

    pub async fn empty_response_retries(&self) -> usize {
        self.agent.lock().await.config.empty_response_retries
    }

    pub async fn estimated_tokens(&self) -> usize {
        let agent = self.agent.lock().await;
        one_core::estimate_tokens(&agent.messages)
    }

    /// Last completion's provider-reported prompt/context size (0 if unknown).
    ///
    /// Prefer this over [`Self::estimated_tokens`] for context-window % (OpenCode-style:
    /// display last API usage, not char/4).
    pub async fn last_prompt_tokens(&self) -> u64 {
        self.agent.lock().await.last_prompt_tokens
    }

    /// Context size for UI / RPC: last provider prompt tokens when available,
    /// otherwise char/4 message estimate. `estimated` is true when falling back.
    pub async fn context_tokens(&self) -> (usize, bool) {
        let agent = self.agent.lock().await;
        if agent.last_prompt_tokens > 0 {
            return (agent.last_prompt_tokens as usize, false);
        }
        (one_core::estimate_tokens(&agent.messages), true)
    }

    /// Provider-reported cumulative usage (input/output) for this runtime.
    pub async fn token_usage(&self) -> one_core::TokenUsage {
        self.agent.lock().await.token_usage
    }
}
