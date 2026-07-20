//! Plan / Act mode transitions and session persistence of mode.

use std::path::PathBuf;
use std::sync::Arc;

use one_core::tool::Tool;
use one_tools::{plan_mode_system_overlay, plan_mode_tools_with_policy};

use super::helpers::new_plan_path;
use super::task_tool::TaskTool;
use super::{AgentMode, AppRuntime};

impl AppRuntime {
    /// Enter plan mode: hard tool gate + system overlay + plan file path.
    pub async fn enter_plan_mode(&mut self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        if self.read_only {
            return Err("already in --read-only; use full tools with /plan instead".into());
        }
        if self.mode == AgentMode::Plan {
            if let Some(path) = &self.plan_path {
                return Ok(path.clone());
            }
        }

        let path = self.plan_path.clone().unwrap_or_else(new_plan_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Seed empty plan file so path is stable and readable.
        if !path.exists() {
            tokio::fs::write(
                &path,
                "# Plan\n\n_Write the implementation plan here._\n",
            )
            .await?;
        }

        {
            let mut state = self.plan_exit.lock().expect("plan exit lock");
            state.plan_path = path.clone();
            state.clear();
        }

        let mut tools: Vec<Arc<dyn Tool>> = plan_mode_tools_with_policy(
            self.path_policy.clone(),
            path.clone(),
            self.plan_exit.clone(),
            Some(self.ask_user_handler.clone()),
        );
        // Explore-only task allowed in plan mode (research while planning).
        if let Some(host) = &self.task_host {
            if host.can_spawn() {
                tools.push(Arc::new(TaskTool::new(host.clone())));
            }
        }
        // Keep extension tools out of plan mode (may be write-capable).

        {
            let mut agent = self.agent.lock().await;
            agent.set_tools(tools);
            agent.config.system_prompt =
                format!("{}{}", self.base_system_prompt, plan_mode_system_overlay(&path));
        }

        self.mode = AgentMode::Plan;
        self.plan_path = Some(path.clone());
        self.persist_mode().await?;
        Ok(path)
    }

    /// Leave plan mode and restore full coding tools (no auto-implement).
    pub async fn leave_plan_mode(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mode == AgentMode::Act {
            return Ok(());
        }
        // Flip mode before rebuild so MCP tools are included.
        self.mode = AgentMode::Act;
        self.apply_act_tools_and_prompt().await?;
        {
            let mut state = self.plan_exit.lock().expect("plan exit lock");
            state.clear();
        }
        self.persist_mode().await?;
        Ok(())
    }

    /// Approve plan and return a user prompt that starts implementation.
    pub async fn approve_plan_prompt(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let plan_path = self
            .plan_path
            .clone()
            .ok_or("no plan file — enter /plan first")?;
        let content = tokio::fs::read_to_string(&plan_path)
            .await
            .unwrap_or_default();
        if content.trim().is_empty()
            || content.trim() == "# Plan\n\n_Write the implementation plan here._"
        {
            return Err("plan file is empty — finish the plan before /act".into());
        }

        self.leave_plan_mode().await?;

        Ok(format!(
            "The plan below was approved. Implement it now. Follow the steps; \
             do not re-plan unless blocked.\n\n\
             Plan file: {}\n\n\
             # Approved plan\n\n\
             {content}",
            plan_path.display()
        ))
    }

    /// Read current plan file contents (if any).
    pub async fn read_plan(&self) -> Option<String> {
        let path = self.plan_path.as_ref()?;
        tokio::fs::read_to_string(path).await.ok()
    }

    pub(super) async fn persist_mode(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(session) = &mut self.session else {
            return Ok(());
        };
        let data = serde_json::json!({
            "mode": self.mode.as_str(),
            "plan_path": self.plan_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        });
        session.append_custom("agent_mode", data).await?;
        Ok(())
    }

    pub(super) fn restore_mode_from_session(&self) -> Option<AgentMode> {
        let session = self.session.as_ref()?;
        // Walk entries newest-first for latest agent_mode custom.
        for entry in session.entries().iter().rev() {
            if let one_session::SessionEntry::Custom {
                custom_type, data, ..
            } = entry
            {
                if custom_type == "agent_mode" {
                    let mode = data
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .and_then(AgentMode::parse)?;
                    return Some(mode);
                }
            }
        }
        None
    }

    pub(super) fn restore_plan_path_from_session(&self) -> Option<PathBuf> {
        let session = self.session.as_ref()?;
        for entry in session.entries().iter().rev() {
            if let one_session::SessionEntry::Custom {
                custom_type, data, ..
            } = entry
            {
                if custom_type == "agent_mode" {
                    return data
                        .get("plan_path")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from);
                }
            }
        }
        None
    }
}
