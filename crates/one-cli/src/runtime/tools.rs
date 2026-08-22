//! Tool list assembly (builtin + extensions + MCP) and MCP sync.
//!
//! Act mode materializes tools from **main AgentSpec.tools** (ToolsSpec) via
//! [`ToolRegistry`], then appends task/job meta-tools when spawn is allowed.

use std::sync::Arc;

use one_core::message::{ContentBlock, TextOrImage, UserContent};
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

    /// Match user query against graph-based intent and reminder rules, injecting JIT `<system-reminder>`.
    ///
    /// Plan mode still injects **Mandatory** safety reminders (e.g. git force-push).
    pub(crate) async fn inject_graph_intent_reminder(
        &mut self,
        query: &str,
    ) -> Option<one_resources::GraphInferenceResult> {
        self.intent_turn = self.intent_turn.saturating_add(1);

        let mut available_tools = Vec::new();
        let mcp_names = self.mcp.server_names();
        if self.mcp.is_disabled() || !mcp_names.is_empty() {
            available_tools.extend(self.main_tool_names_preview());
            available_tools.extend(mcp_names);
        }

        let opts = one_resources::InferOptions {
            entity_params: std::collections::HashMap::new(),
            available_tools,
            turn_index: self.intent_turn,
            reminder_last_turn: self.intent_reminder_last_turn.clone(),
            mandatory_only: self.mode == AgentMode::Plan,
        };

        let graph = self.intent_graph.read().await;
        let result = graph.infer_with(query, &opts);
        drop(graph);

        if let Some(rendered) = result.render_reminder_markdown() {
            for rem in &result.active_reminders {
                self.intent_reminder_last_turn
                    .insert(rem.reminder_id.clone(), self.intent_turn);
            }
            let text = one_core::system_reminder(rendered);
            let agent = self.agent.lock().await;
            agent.push_notification(text);
            Some(result)
        } else {
            None
        }
    }

    /// Path to user-global custom intent graph JSON file.
    pub fn custom_intent_graph_path(&self) -> std::path::PathBuf {
        one_session::agent_dir()
            .join("intent_graph")
            .join("custom.json")
    }

    /// Learn and persist a new intent rule from user instruction or structured text.
    pub async fn learn_intent_from_text(
        &mut self,
        text: &str,
    ) -> Result<one_resources::LearnedRuleSummary, Box<dyn std::error::Error>> {
        let mut graph = self.intent_graph.write().await;
        let summary = graph
            .learn_from_text(text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let path = self.custom_intent_graph_path();
        graph.save_custom_to_file(&path)?;
        Ok(summary)
    }

    /// Learn and persist a new intent rule from the current session's latest turn trajectory.
    pub async fn learn_intent_from_session(
        &mut self,
    ) -> Result<one_resources::LearnedRuleSummary, Box<dyn std::error::Error>> {
        let agent = self.agent.lock().await;
        let mut last_user_query = None;
        let mut tools_used = Vec::new();

        for msg in agent.messages.iter().rev() {
            match msg {
                one_core::AgentMessage::User(u) => {
                    let extracted = match &u.content {
                        UserContent::Text(t) => Some(t.clone()),
                        UserContent::Blocks(b) => {
                            let mut txt = String::new();
                            for block in b {
                                if let TextOrImage::Text { text } = block {
                                    txt.push_str(text);
                                }
                            }
                            if !txt.is_empty() {
                                Some(txt)
                            } else {
                                None
                            }
                        }
                    };

                    if let Some(text) = extracted {
                        let trimmed = text.trim();
                        // Skip internal system notifications/reminders pushed as user messages
                        let is_system_meta = trimmed.starts_with("<system-reminder>")
                            || trimmed.starts_with("<env>")
                            || trimmed.starts_with("<context>")
                            || trimmed.starts_with("<memory-catalog>")
                            || trimmed.contains("### Learned Tool Intent")
                            || trimmed.contains("### Graph Intent Guidance");

                        if !is_system_meta && !trimmed.is_empty() {
                            last_user_query = Some(trimmed.to_string());
                            break;
                        }
                    }
                }
                one_core::AgentMessage::Assistant(a) => {
                    for block in &a.content {
                        if let ContentBlock::ToolCall { name, .. } = block {
                            if !tools_used.contains(name) {
                                tools_used.push(name.clone());
                            }
                        }
                    }
                }
                one_core::AgentMessage::ToolResult(tr) => {
                    if !tools_used.contains(&tr.tool_name) {
                        tools_used.push(tr.tool_name.clone());
                    }
                }
            }
        }
        drop(agent);

        let query = match last_user_query {
            Some(q) if !q.trim().is_empty() => q,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "当前会话中未找到用户提问，请先进行对话或直接输入规则文本：/learn <规则>",
                )
                .into());
            }
        };

        let mut graph = self.intent_graph.write().await;
        let summary = graph
            .learn_from_interaction(&query, &tools_used, None)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let path = self.custom_intent_graph_path();
        graph.save_custom_to_file(&path)?;
        Ok(summary)
    }

    /// List all custom learned rules currently loaded in the intent graph.
    pub async fn list_learned_intent_rules(&self) -> Vec<one_resources::LearnedRuleSummary> {
        self.intent_graph.read().await.list_custom_rules()
    }

    /// Clear all custom learned rules and reset graph back to built-ins.
    pub async fn reset_custom_intent_rules(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = self.intent_graph.write().await;
        graph.clear_custom_rules();
        let path = self.custom_intent_graph_path();
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    /// Get current intent graph statistics (nodes, edges, custom_rules, triggers).
    pub async fn intent_graph_stats(&self) -> (usize, usize, usize, usize) {
        let graph = self.intent_graph.read().await;
        let total_nodes = graph.nodes.len();
        let total_edges = graph.edges.len();
        let custom_rules = graph.list_custom_rules().len();
        let triggers = graph
            .nodes
            .values()
            .filter(|n| matches!(n, one_resources::GraphNode::Trigger { .. }))
            .count();
        (total_nodes, total_edges, custom_rules, triggers)
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
