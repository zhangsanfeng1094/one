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
    ///
    /// Job poll/wait/kill also accept bash `bg_*` ids (unified task surface).
    pub(super) fn push_task_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        if !self.should_register_task_tools() {
            return;
        }
        let Some(host) = &self.task_host else {
            return;
        };
        let bash = self.bg_registry.clone();
        tools.push(Arc::new(TaskTool::new(host.clone())));
        tools.push(Arc::new(JobOutputTool::with_bash(
            host.jobs(),
            bash.clone(),
        )));
        tools.push(Arc::new(WaitTasksTool::with_bash(
            host.jobs(),
            bash.clone(),
        )));
        tools.push(Arc::new(JobKillTool::with_bash(host.jobs(), bash)));
    }

    pub(super) async fn apply_act_tools_and_prompt(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.recompose_base_prompt();
        self.rebuild_act_tools().await?;
        let mut agent = self.agent.lock().await;
        agent.config.system_prompt = self.effective_system_prompt();
        Ok(())
    }

    /// ToolsSpec that drives the live main session (CLI read_only overrides).
    pub(super) fn effective_main_tools_spec(&self) -> ToolsSpec {
        if self.read_only {
            let mut t = ToolsSpec::read_only();
            // Keep ask_user for interactive main; deny only if main asked.
            if self.main_agent.tools.deny.iter().any(|d| d == "ask_user") {
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
            todo_state: self.todo_state.clone(),
            memory_lookups: self.memory_lookups.clone(),
            // Pi style: hosted search is server-side on the main request — never
            // a second-hop backend on the local function tool.
            #[cfg(feature = "network")]
            backend_web_search: None,
        };
        let mut registry = ToolRegistry::with_builtins();
        let ext = self.extensions.tools();
        registry.register_instances(ext.iter().cloned());
        // Deferred (default): search_tool + use_tool only.
        // Direct: full MCP tool schemas. Plan mode: nothing.
        let mcp_tools = if self.mode != AgentMode::Plan {
            self.mcp.model_visible_tools()
        } else {
            vec![]
        };
        registry.register_instances(mcp_tools.iter().cloned());

        let tools_spec = self.effective_main_tools_spec();
        // When main tools.mcp is true, registered MCP / meta tools are appended.
        // When false, materialize_tools strips MCP-looking names and meta tools.
        let mut tools = materialize_tools(&tools_spec, &registry, &ctx, false)
            .map_err(|e| format!("main tools materialize failed: {e}"))?;

        // Feature `memory` master switch: L2 tools only when package is on.
        let settings = crate::settings::load();
        let mem_opts = super::features::effective_memory_options(&self.applied_features, &settings);
        if mem_opts.enabled
            && !tools_spec.deny.iter().any(|d| d == "memory_search")
            && (tools_spec.allow.is_empty()
                || tools_spec.allow.iter().any(|a| a == "memory_search"))
        {
            tools.push(std::sync::Arc::new(
                super::memory_search_tool::MemorySearchTool::new(
                    self.resources.agent_dir.clone(),
                    self.cwd.clone(),
                ),
            ));
        }
        // memory_write: same package + write permission + not read-only.
        if mem_opts.enabled
            && mem_opts.write_enabled
            && !self.read_only
            && !tools_spec.deny.iter().any(|d| d == "memory_write")
            && (tools_spec.allow.is_empty() || tools_spec.allow.iter().any(|a| a == "memory_write"))
        {
            tools.push(std::sync::Arc::new(
                super::memory_write_tool::MemoryWriteTool::new(
                    self.resources.agent_dir.clone(),
                    self.cwd.clone(),
                ),
            ));
        }

        // Extensions always available in Act (unless ToolsSpec deny listed them —
        // materialize won't include them unless in allow/extra when allow non-empty).
        // If profile coding with empty allow, builtins only — re-append ext not in list.
        if tools_spec.allow.is_empty()
            && matches!(
                tools_spec.profile,
                ToolProfile::Coding | ToolProfile::ReadOnly | ToolProfile::None
            )
        {
            let existing: std::collections::HashSet<_> =
                tools.iter().map(|t| t.definition().name).collect();
            for t in ext {
                let n = t.definition().name;
                if !existing.contains(&n) && !tools_spec.deny.iter().any(|d| d == &n) {
                    tools.push(t);
                }
            }
        }

        // When we inject hosted web_search, drop the same-named local function
        // (server wins — pi-xai mergeXaiTools). Feature off → keep local.
        if self.hosted_search_active() {
            tools.retain(|t| t.definition().name != "web_search");
        }

        self.push_task_tools(&mut tools);
        self.mcp_tools_generation = self.mcp.generation();

        // Keep child harness MCP/ext set in sync.
        self.refresh_task_dynamic_tools().await;

        let system_prompt = self.effective_system_prompt();
        let mut agent = self.agent.lock().await;
        agent.set_tools(tools);
        // Refresh MCP announcement when the connected set changes (deferred mode).
        agent.config.system_prompt = system_prompt;
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

    pub(crate) async fn inject_mcp_reminder(&mut self) {
        if self.mcp.is_disabled() || self.mode == AgentMode::Plan {
            return;
        }
        let snapshot = self.mcp.catalog().status_snapshot();
        let Some(reminder) = self.mcp_reminder_state.next(&snapshot) else {
            return;
        };
        let text = one_core::system_reminder(reminder.body);
        let agent = self.agent.lock().await;
        agent.push_notification(text);
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
        let generation = self.mcp.generation();
        if generation == self.mcp_tools_generation {
            return Ok(());
        }
        tracing::debug!(
            from = self.mcp_tools_generation,
            to = generation,
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
