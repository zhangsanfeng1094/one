//! Hot-reload resources, extensions, plugins; skills enable/disable.

use std::sync::Arc;

use one_ext::{discover_all, ExtensionContext};
use one_resources::ResourceLoader;
use one_session::agent_dir;
use one_tools::{plan_mode_system_overlay, plan_mode_tools_with_policy};

use super::helpers::new_plan_path;
use super::{AgentMode, AppRuntime};

impl AppRuntime {
    pub async fn reload_extensions(&mut self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let agent_dir = agent_dir();
        // Unload previous extensions.
        {
            let data = self.extensions.data().clone();
            let session_path = self.session_path();
            let ctx = ExtensionContext {
                cwd: &self.cwd,
                session_file: session_path.as_deref(),
                data: &data,
            };
            let _ = self.extensions.unload_all(&ctx).await;
        }

        // Reload resources (skills/prompts/AGENTS) + extensions + plugins.
        let mut resources = ResourceLoader::discover(&self.cwd, &agent_dir).await?;
        let discovery = discover_all(&self.cwd, &agent_dir).await?;
        if !discovery.skill_dirs.is_empty() {
            if let Ok(extra) = one_resources::discover_skills(&discovery.skill_dirs).await {
                resources.merge_skills(extra);
            }
        }
        let _ = resources.merge_prompt_dirs(&discovery.prompt_dirs).await;
        for overlay in &discovery.system_overlays {
            resources.push_system_append(overlay.clone());
        }
        let user_settings = crate::settings::load();
        resources.apply_skills_config(&user_settings.skills_config_entries());
        self.resources = resources;

        let extensions = Arc::new(discovery.runtime);
        {
            let data = extensions.data().clone();
            let session_path = self.session_path();
            let ctx = ExtensionContext {
                cwd: &self.cwd,
                session_file: session_path.as_deref(),
                data: &data,
            };
            extensions.load_all(&ctx).await?;
        }
        if let Some(overlay) = extensions.system_prompt_overlay() {
            self.resources.push_system_append(overlay);
        }
        self.extensions = extensions;

        // Re-wire gate + hooks onto the agent.
        {
            let mut agent = self.agent.lock().await;
            agent.set_tool_gate(Some(
                self.extensions
                    .tool_gate(self.permission_gate.clone()),
            ));
            agent.set_hooks(Some(self.extensions.agent_hooks()));
        }

        // MCP: re-read disk config + merge plugin servers; keep live pool.
        if !self.mcp.is_disabled() {
            if let Err(e) = self.mcp.reload_from_disk(&self.cwd) {
                tracing::warn!(error = %e, "MCP reload from disk failed");
            }
            if !discovery.plugin_mcp_servers.is_empty() {
                self.mcp
                    .merge_plugin_server_json(&discovery.plugin_mcp_servers);
            }
        }

        self.base_system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        // Rebuild tools + prompt for current mode.
        match self.mode {
            AgentMode::Plan => {
                let path = self.plan_path.clone().unwrap_or_else(new_plan_path);
                self.plan_path = Some(path.clone());
                {
                    let mut state = self.plan_exit.lock().expect("plan exit lock");
                    state.plan_path = path.clone();
                }
                let tools = plan_mode_tools_with_policy(
                    self.path_policy.clone(),
                    path.clone(),
                    self.plan_exit.clone(),
                    Some(self.ask_user_handler.clone()),
                );
                let mut agent = self.agent.lock().await;
                agent.set_tools(tools);
                agent.config.system_prompt =
                    format!("{}{}", self.base_system_prompt, plan_mode_system_overlay(&path));
            }
            AgentMode::Act => {
                self.apply_act_tools_and_prompt().await?;
            }
        }
        Ok(self.extensions.names())
    }

    /// Re-apply skills enable/disable from settings and rebuild system prompt.
    pub async fn reapply_skills_config(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let user_settings = crate::settings::load();
        self.resources
            .apply_skills_config(&user_settings.skills_config_entries());
        self.base_system_prompt = self
            .resources
            .build_system_prompt(one_core::agent::DEFAULT_SYSTEM_PROMPT);
        match self.mode {
            AgentMode::Plan => {
                let path = self.plan_path.clone().unwrap_or_else(new_plan_path);
                let mut agent = self.agent.lock().await;
                agent.config.system_prompt =
                    format!("{}{}", self.base_system_prompt, plan_mode_system_overlay(&path));
            }
            AgentMode::Act => {
                let mut agent = self.agent.lock().await;
                agent.config.system_prompt = self.base_system_prompt.clone();
            }
        }
        Ok(())
    }

    /// Toggle a skill on/off (persists to settings.json), returns new enabled state.
    pub async fn set_skill_enabled(
        &mut self,
        path: &std::path::Path,
        enabled: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let mut s = crate::settings::load();
        s.set_skill_enabled(path, enabled);
        crate::settings::save(&s)?;
        self.reapply_skills_config().await?;
        Ok(enabled)
    }
}
