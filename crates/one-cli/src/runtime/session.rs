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
        // Ensure any MCP servers that finished loading attach to this clean slate.
        self.sync_mcp_tools().await?;
        if switching {
            self.extensions.notify_session_start().await;
        }
        Ok(())
    }

    pub async fn open_session_path(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let switching = self.session.is_some();
        if switching {
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
        }
        load_extension_state(self.extensions.as_ref(), &session);
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
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        Ok(SessionManager::list(&self.cwd).await?)
    }

    pub async fn set_session_name(
        &mut self,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(session) = &mut self.session {
            session.append_session_info(name).await?;
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
            session
                .append_thinking_level_change(level.as_str())
                .await?;
        }
        Ok(())
    }

    pub async fn thinking_level(&self) -> ThinkingLevel {
        self.agent.lock().await.config.thinking_level
    }

    pub async fn estimated_tokens(&self) -> usize {
        let agent = self.agent.lock().await;
        one_core::estimate_tokens(&agent.messages)
    }

    /// Provider-reported cumulative usage (input/output) for this runtime.
    pub async fn token_usage(&self) -> one_core::TokenUsage {
        self.agent.lock().await.token_usage
    }
}
