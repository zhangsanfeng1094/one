//! Tool list assembly (builtin + extensions + MCP) and MCP sync.
//!
//! Act mode materializes tools from **main AgentSpec.tools** (ToolsSpec) via
//! [`ToolRegistry`], then appends task/job meta-tools when spawn is allowed.

use std::sync::Arc;

use one_core::tool::Tool;
use one_tools::{ToolBuildContext, ToolRegistry};

use super::job_tools::{JobKillTool, JobOutputTool, WaitTasksTool};
use super::task_tool::TaskTool;
use super::tool_materialize::{materialize_tools, resolve_names};
use super::{AgentMode, AppRuntime};
use crate::protocol::{ToolProfile, ToolsSpec};

impl AppRuntime {
    /// Whether task/job tools should be registered under current applied features.
    pub(super) fn should_register_task_tools(&self) -> bool {
        self.applied_features.subagent_enabled()
            && self
                .task_host
                .as_ref()
                .map(|h| h.can_spawn())
                .unwrap_or(false)
    }

    /// Append task + job poll/kill tools when the feature + spawn policy allow.
    pub(super) fn push_task_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        if !self.should_register_task_tools() {
            return;
        }
        let Some(host) = &self.task_host else {
            return;
        };
        tools.push(Arc::new(TaskTool::new(host.clone())));
        tools.push(Arc::new(JobOutputTool::new(host.jobs())));
        tools.push(Arc::new(WaitTasksTool::new(host.jobs())));
        tools.push(Arc::new(JobKillTool::new(host.jobs())));
    }

    pub(super) async fn apply_act_tools_and_prompt(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.recompose_base_prompt();
        self.rebuild_act_tools().await?;
        let mut agent = self.agent.lock().await;
        agent.config.system_prompt = self.base_system_prompt.clone();
        Ok(())
    }

    /// ToolsSpec that drives the live main session (CLI read_only overrides).
    pub(super) fn effective_main_tools_spec(&self) -> ToolsSpec {
        if self.read_only {
            let mut t = ToolsSpec::read_only();
            // Keep ask_user for interactive main; deny only if main asked.
            if self
                .main_agent
                .tools
                .deny
                .iter()
                .any(|d| d == "ask_user")
            {
                t.deny.push("ask_user".into());
            }
            t.mcp = false;
            return t;
        }
        self.main_agent.tools.clone()
    }

    /// Rebuild the Act-mode tool list from main AgentSpec.tools + MCP/ext.
    pub(super) async fn rebuild_act_tools(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let ctx = ToolBuildContext {
            policy: self.path_policy.clone(),
            auto_approve: self.auto_approve,
            bg_registry: self.bg_registry.clone(),
            ask_user: Some(self.ask_user_handler.clone()),
            tool_gate: Some(self.permission_gate.clone()),
        };
        let mut registry = ToolRegistry::with_builtins();
        let ext = self.extensions.tools();
        registry.register_instances(ext.iter().cloned());
        let mcp_tools = if self.mode != AgentMode::Plan {
            self.mcp.tools()
        } else {
            vec![]
        };
        registry.register_instances(mcp_tools.iter().cloned());

        let tools_spec = self.effective_main_tools_spec();
        // When main tools.mcp is true, registered MCP instances are appended.
        // When false, materialize_tools strips MCP-looking names.
        let mut tools = materialize_tools(&tools_spec, &registry, &ctx, false).map_err(|e| {
            format!("main tools materialize failed: {e}")
        })?;

        // Extensions always available in Act (unless ToolsSpec deny listed them —
        // materialize won't include them unless in allow/extra when allow non-empty).
        // If profile coding with empty allow, builtins only — re-append ext not in list.
        if tools_spec.allow.is_empty() && matches!(tools_spec.profile, ToolProfile::Coding | ToolProfile::ReadOnly | ToolProfile::None) {
            let existing: std::collections::HashSet<_> =
                tools.iter().map(|t| t.definition().name).collect();
            for t in ext {
                let n = t.definition().name;
                if !existing.contains(&n)
                    && !tools_spec.deny.iter().any(|d| d == &n)
                {
                    tools.push(t);
                }
            }
        }

        self.push_task_tools(&mut tools);
        self.mcp_tools_generation = self.mcp.generation();

        // Keep child harness MCP/ext set in sync.
        self.refresh_task_dynamic_tools().await;

        let mut agent = self.agent.lock().await;
        agent.set_tools(tools);
        // Keep shared queue: bash + agent jobs (already set at build; re-apply if missing).
        if !self.read_only {
            if let Some(host) = &self.task_host {
                agent.set_notification_queue(host.jobs().notification_queue());
            } else {
                agent.set_notification_queue(self.bg_registry.notification_queue());
            }
        } else if self.should_register_task_tools() {
            if let Some(host) = &self.task_host {
                agent.set_notification_queue(host.jobs().notification_queue());
            }
        }
        Ok(())
    }

    /// Preview resolved main tool names (for status / debug).
    pub fn main_tool_names_preview(&self) -> Vec<String> {
        let spec = self.effective_main_tools_spec();
        resolve_names(&spec, false)
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
