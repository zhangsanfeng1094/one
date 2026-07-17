//! Tool list assembly (builtin + extensions + MCP) and MCP sync.

use std::sync::Arc;

use one_core::tool::Tool;
use one_tools::{
    coding_tools_with_options, read_only_tools_with_ask, ToolBuildOptions,
};

use super::{AgentMode, AppRuntime};

impl AppRuntime {
    pub(super) async fn apply_act_tools_and_prompt(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Refresh base prompt from resources in case of /reload.
        self.base_system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);

        self.rebuild_act_tools().await?;
        let mut agent = self.agent.lock().await;
        agent.config.system_prompt = self.base_system_prompt.clone();
        if !self.read_only {
            agent.set_notification_queue(self.bg_registry.notification_queue());
        }
        Ok(())
    }

    /// Rebuild the Act-mode tool list (builtin + extensions + current MCP snapshot).
    pub(super) async fn rebuild_act_tools(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut tools: Vec<Arc<dyn Tool>> = if self.read_only {
            read_only_tools_with_ask(
                self.path_policy.clone(),
                Some(self.ask_user_handler.clone()),
            )
        } else {
            coding_tools_with_options(ToolBuildOptions {
                policy: self.path_policy.clone(),
                auto_approve: self.auto_approve,
                registry: self.bg_registry.clone(),
                ask_user: Some(self.ask_user_handler.clone()),
            })
        };
        tools.extend(self.extensions.tools());
        // MCP tools only outside Plan mode (external side effects).
        if self.mode != AgentMode::Plan {
            tools.extend(self.mcp.tools());
        }
        self.mcp_tools_generation = self.mcp.generation();

        let mut agent = self.agent.lock().await;
        agent.set_tools(tools);
        Ok(())
    }

    /// If background MCP load advanced, re-apply tools onto the agent.
    ///
    /// Called before each prompt so tools that finished mid-session become
    /// available on the next turn without reconnecting (Grok shared-pool model).
    pub async fn sync_mcp_tools(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp.is_disabled() {
            return Ok(());
        }
        if self.mode == AgentMode::Plan {
            // Stay off MCP tools in plan mode even if pool is ready.
            return Ok(());
        }
        let gen = self.mcp.generation();
        if gen == self.mcp_tools_generation {
            return Ok(());
        }
        tracing::debug!(
            from = self.mcp_tools_generation,
            to = gen,
            tools = self.mcp.tool_count(),
            "syncing MCP tools into agent"
        );
        self.rebuild_act_tools().await
    }

    /// Enable/disable an MCP server (persists + reconnects or drops tools).
    pub async fn set_mcp_server_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.mcp.set_server_enabled(name, enabled).await?;
        // Reflect tool list change on the agent immediately.
        self.sync_mcp_tools().await?;
        Ok(())
    }

    /// Import foreign MCP servers into One config and connect them live.
    pub async fn import_mcp_from_agents(
        &mut self,
        names: &[String],
        source: Option<one_mcp::ConfigSourceKind>,
        overwrite: bool,
    ) -> Result<one_mcp::ImportReport, Box<dyn std::error::Error>> {
        let report = self
            .mcp
            .import_from_agents(&self.cwd, names, source, overwrite)
            .await?;
        self.sync_mcp_tools().await?;
        Ok(report)
    }
}
