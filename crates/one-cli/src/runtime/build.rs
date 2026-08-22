//! Cold-start assembly of [`super::AppRuntime`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use one_core::agent::{Agent, AgentConfig, ThinkingLevel};
use one_core::tool::Tool;
use one_ext::{discover_all, ExtensionContext};
use one_mcp::{McpManager, McpReminderState};
use one_resources::ResourceLoader;
use one_session::{agent_dir, SessionManager};
use one_tools::{
    coding_tools_with_options, plan_mode_system_overlay, plan_mode_tools_with_policy,
    read_only_tools_with_ask, AskUserHandler, BackgroundTaskRegistry, OsSandbox, PermissionMode,
    PermissionRules, PlanExitState, ToolBuildOptions,
};

use super::features::{env_no_subagent, FeatureState};
use super::helpers::{load_extension_state, new_plan_path};
use super::job_tools::{JobKillTool, JobOutputTool, WaitTasksTool};
use super::policy::build_path_policy;
use super::prompt_compose::compose_base_system_prompt;
use super::task_tool::{
    harness_opts_from_policy, main_parent_agent_spec_for_cwd, TaskTool, TaskToolHost,
};
use super::{AgentMode, AppRuntime};
use crate::approval::PermissionGate;
use crate::cli::{Cli, RunMode};
use crate::hitl::{HitlChannel, InteractiveAskUser};

impl AppRuntime {
    pub async fn build(cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone());
        let agent_dir = agent_dir();

        let mut resources = ResourceLoader::discover(&cwd, &agent_dir).await?;

        // Codex-inspired: extensions.json + plugins + external hooks.
        let discovery = discover_all(&cwd, &agent_dir).await?;
        if !discovery.skill_dirs.is_empty() {
            match one_resources::discover_skills(&discovery.skill_dirs).await {
                Ok(extra) => resources.merge_skills(extra),
                Err(e) => tracing::warn!(error = %e, "plugin skill discovery failed"),
            }
        }
        if let Err(e) = resources.merge_prompt_dirs(&discovery.prompt_dirs).await {
            tracing::warn!(error = %e, "plugin prompt discovery failed");
        }
        for overlay in &discovery.system_overlays {
            resources.push_system_append(overlay.clone());
        }

        // Harness / isolation: omit skills catalog + skill force-load.
        let disable_skills = cli.no_skills
            || std::env::var_os("ONE_DISABLE_SKILLS").is_some_and(|v| v != "0" && v != "false");
        if disable_skills {
            tracing::info!("skills disabled (--no-skills / ONE_DISABLE_SKILLS)");
            resources.clear_skills();
        }

        let extensions = Arc::new(discovery.runtime);
        {
            let data = extensions.data().clone();
            let ctx = ExtensionContext {
                cwd: &cwd,
                session_file: None,
                data: &data,
            };
            extensions.load_all(&ctx).await?;
        }
        if let Some(overlay) = extensions.system_prompt_overlay() {
            resources.push_system_append(overlay);
        }

        let user_settings = crate::settings::load();
        // OpenCode-style unified tool_output limits (settings + env).
        user_settings.apply_tool_output_limits();
        // Prune spilled tool outputs older than 7 days (OpenCode Truncate.cleanup).
        {
            let report = one_tools::cleanup_tool_outputs(one_tools::TOOL_OUTPUT_RETENTION_DAYS);
            if report.removed_files > 0 || report.removed_dirs > 0 {
                tracing::info!(
                    removed_files = report.removed_files,
                    removed_bytes = report.removed_bytes,
                    removed_dirs = report.removed_dirs,
                    errors = report.errors,
                    "tool-outputs cleanup"
                );
            }
        }
        // Codex-style skills enable/disable (settings.skills_config).
        resources.apply_skills_config(&user_settings.skills_config_entries());
        let no_subagent_process = cli.no_subagent || env_no_subagent();
        let no_memory_process = cli.no_memory || super::features::env_no_memory();
        let applied_features = FeatureState::from_settings(&user_settings)
            .with_process_overrides(no_subagent_process, no_memory_process);
        if no_subagent_process {
            tracing::info!("subagent feature disabled (--no-subagent / ONE_DISABLE_SUBAGENT)");
        }
        if no_memory_process {
            tracing::info!("memory feature disabled (--no-memory / ONE_NO_MEMORY)");
        }
        let mut perm_mode = if cli.always_approve {
            PermissionMode::BypassPermissions
        } else if let Some(m_str) = &cli.permission_mode {
            PermissionMode::parse(m_str).unwrap_or(PermissionMode::Default)
        } else if cli.auto_approve {
            PermissionMode::BypassPermissions
        } else if let Some(m_str) = &user_settings.permission_mode {
            PermissionMode::parse(m_str).unwrap_or(PermissionMode::Default)
        } else if user_settings.auto_approve == Some(true) {
            PermissionMode::BypassPermissions
        } else {
            PermissionMode::Default
        };

        if perm_mode.is_always_approve() {
            if let Err(err) = crate::governance::check_bypass_permissions_allowed() {
                tracing::warn!("{err}; falling back to default permission mode");
                perm_mode = PermissionMode::Default;
            }
        }

        let auto_approve = perm_mode.is_always_approve();

        // agentskills.io: allowlist skill dirs so progressive disclosure `read` works
        // (Codex-compatible: ~/.agents/skills + client skill homes + package dirs).
        let path_policy = build_path_policy(&cwd, cli, &user_settings, &resources);
        // Optional settings kill-switch for OS bash sandbox.
        if user_settings.bash_sandbox == Some(false) {
            std::env::set_var("ONE_BASH_SANDBOX", "0");
        }

        // Shared notification queue: background bash + agent jobs → Agent drain.
        let shared_notifications = Arc::new(Mutex::new(Vec::<String>::new()));
        let bg_registry = Arc::new(BackgroundTaskRegistry::with_notification_queue(
            shared_notifications.clone(),
        ));
        // Apply OS sandbox to background tasks even before BashTool construction.
        bg_registry.set_os_sandbox(OsSandbox::from_policy(&path_policy));
        let agent_jobs = super::jobs::AgentJobRegistry::new(shared_notifications.clone());

        // ACP needs the same interactive PermissionGate / HitlChannel so tool
        // approvals and ask_user can be bridged to the client over JSON-RPC.
        let interactive = matches!(cli.mode, RunMode::Interactive | RunMode::Acp);
        let perm_rules = user_settings
            .permissions
            .clone()
            .unwrap_or_else(PermissionRules::default);
        let permission_gate = PermissionGate::with_permission_mode_and_policy(
            perm_rules,
            perm_mode,
            interactive,
            Some(path_policy.clone()),
        );

        let hitl = HitlChannel::new(interactive);
        let ask_user_handler: Arc<dyn AskUserHandler> =
            Arc::new(InteractiveAskUser::new(hitl.clone()));

        let start_plan = cli.plan && !cli.read_only;
        let plan_path = if start_plan {
            Some(new_plan_path())
        } else {
            None
        };
        let plan_exit = Arc::new(Mutex::new(PlanExitState::new(
            plan_path
                .clone()
                .unwrap_or_else(|| agent_dir.join("plans").join("_none.md")),
        )));

        // Main AgentSpec drives Act tool face + task spawn table.
        let main_agent = main_parent_agent_spec_for_cwd(&cwd);

        // Task meta-tool host (same harness as `one agent run`). Enabled for
        // main agents that can spawn (not pure --read-only research shells).
        let task_host = {
            let add_dirs = cli.add_dir.clone();
            let opts =
                harness_opts_from_policy(cwd.clone(), cli.full_access, add_dirs, auto_approve);
            Some(TaskToolHost::new(
                opts,
                main_agent.clone(),
                agent_jobs.clone(),
                path_policy.clone(),
            ))
        };

        let mut tools: Vec<Arc<dyn Tool>> = if cli.read_only {
            // No bash / background tools in read-only mode.
            // Still allow explore task so -RO parents can delegate research.
            read_only_tools_with_ask(path_policy.clone(), Some(ask_user_handler.clone()))
        } else if start_plan {
            plan_mode_tools_with_policy(
                path_policy.clone(),
                plan_path.clone().expect("plan path"),
                plan_exit.clone(),
                Some(ask_user_handler.clone()),
            )
        } else {
            coding_tools_with_options(ToolBuildOptions {
                policy: path_policy.clone(),
                auto_approve,
                registry: bg_registry.clone(),
                ask_user: Some(ask_user_handler.clone()),
                // Same gate the agent uses pre-tool — enables escalate_on_failure.
                tool_gate: Some(permission_gate.clone()),
            })
        };
        // Register `task` + job poll/kill when feature + spawn policy allow.
        let can_spawn = task_host.as_ref().map(|h| h.can_spawn()).unwrap_or(false);
        if applied_features.subagent_enabled() && can_spawn {
            if let Some(host) = &task_host {
                tools.push(Arc::new(TaskTool::new(host.clone())));
                tools.push(Arc::new(JobOutputTool::new(host.jobs())));
                tools.push(Arc::new(WaitTasksTool::new(host.jobs())));
                tools.push(Arc::new(JobKillTool::new(host.jobs())));
            }
        }
        // Extension tools only in Act mode (may include write-capable tools).
        if !start_plan {
            tools.extend(extensions.tools());
        }
        // MCP: Grok-style — load config sync, connect servers in background.
        // Do not block TUI / first paint on cold `npx` downloads.
        // Plan mode still starts the pool (so /act gets tools) but does not register them yet.
        // Plugin `mcpServers` merge in after spawn (One user/project names win).
        let disable_mcp = cli.no_mcp
            || std::env::var_os("ONE_DISABLE_MCP").is_some_and(|v| v != "0" && v != "false");
        let mcp = if disable_mcp {
            McpManager::empty()
        } else {
            match McpManager::spawn(&cwd) {
                Ok(m) => {
                    if !discovery.plugin_mcp_servers.is_empty() {
                        m.merge_plugin_server_json(&discovery.plugin_mcp_servers);
                    }
                    if m.is_loading() {
                        tracing::info!("MCP background connect started");
                    }
                    m
                }
                Err(e) => {
                    tracing::warn!(error = %e, "MCP config load failed; continuing without MCP");
                    McpManager::empty()
                }
            }
        };
        // Snapshot model-visible MCP tools (deferred: meta tools; direct: full set).
        // Usually only meta tools right after spawn; real tools appear via sync_mcp_tools.
        if !start_plan {
            tools.extend(mcp.model_visible_tools());
        }
        let mcp_tools_generation = mcp.generation();

        // Session-frozen env + memory L2 (recomputed on /new and /reload only).
        // Feature `memory` is the package master switch (also honors --no-memory / env).
        let env_context = super::env_context::build_env_context(&cwd);
        let memory_opts =
            super::features::effective_memory_options(&applied_features, &user_settings);
        let memory_catalog = if memory_opts.enabled {
            one_resources::load_memory_catalog(&agent_dir, &cwd, &memory_opts)
                .await
                .map(|c| c.prompt_section)
        } else {
            None
        };

        let base_system_prompt =
            compose_base_system_prompt(super::prompt_compose::ComposeBaseInput {
                features: &applied_features,
                resources: &resources,
                can_spawn,
                env_context: Some(env_context.as_str()),
                memory_catalog: memory_catalog.as_deref(),
            });
        let system_prompt = if start_plan {
            let p = plan_path.as_ref().expect("plan path");
            format!("{base_system_prompt}{}", plan_mode_system_overlay(p))
        } else {
            base_system_prompt.clone()
        };
        let max_turns = cli.max_turns.max(1);
        let empty_response_retries = user_settings.empty_response_retries();
        let mut agent = Agent::new(
            AgentConfig {
                system_prompt,
                max_turns,
                thinking_level: ThinkingLevel::Off,
                // Set properly after refresh_web_search_backend (capability + feature).
                server_search: false,
                empty_response_retries,
            },
            tools,
        );
        // Extension PreToolUse → PermissionGate → after_tool (Codex-style pipeline).
        agent.set_tool_gate(Some(extensions.tool_gate(permission_gate.clone())));
        agent.set_hooks(Some(extensions.agent_hooks()));
        // Optional Langfuse trace (additive; default off).
        // session_id is filled in after the session is opened (see below).
        let mut langfuse_sink = None;
        if trace_enabled(cli) {
            match crate::langfuse::LangfuseConfig::from_env() {
                Some(cfg) => {
                    let host = cfg.project_url_hint();
                    tracing::info!(%host, "langfuse tracing enabled");
                    let sink = crate::langfuse::LangfuseTraceSink::start(cfg);
                    agent.set_trace(Some(sink.clone()));
                    agent.set_trace_meta(one_core::TraceRunMeta {
                        agent_version: Some(env!("CARGO_PKG_VERSION").into()),
                        config: Some(serde_json::json!({
                            "max_turns": max_turns,
                            "read_only": cli.read_only,
                            "plan": cli.plan,
                            "auto_approve": auto_approve,
                            "cwd": cwd.display().to_string(),
                            "trace_full": cli.trace_full,
                            "backend": "langfuse",
                        })),
                        task_id: None,
                        session_id: None,
                        user_id: crate::langfuse::user_id_from_env(),
                        trace_full: cli.trace_full,
                    });
                    eprintln!("trace: langfuse otel ({host}/api/public/otel/v1/traces)");
                    if cli.trace_full {
                        eprintln!("trace: full I/O previews enabled (--trace-full)");
                    }
                    // Subagents nest under parent `task` tool spans via this host handle.
                    if let Some(host) = &task_host {
                        host.set_langfuse(Some(sink.clone()), cli.trace_full).await;
                    }
                    langfuse_sink = Some(sink);
                }
                None => {
                    eprintln!(
                        "trace: requested but Langfuse keys missing — set LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY"
                    );
                    tracing::warn!(
                        "--trace/--langfuse ignored: Langfuse credentials not configured"
                    );
                }
            }
        }
        // Claude-style: completed background bash + agent jobs → conversation notice.
        // Wire when coding tools or subagent task tools are active.
        let wire_notifications =
            !cli.read_only || (applied_features.subagent_enabled() && can_spawn);
        if wire_notifications {
            agent.set_notification_queue(shared_notifications);
        }

        // Interactive `-r` opens a picker in TUI — don't load a session yet.
        let pick_session = cli.resume
            && matches!(cli.mode, crate::cli::RunMode::Interactive)
            && cli.print.is_none()
            && cli.session.is_none();

        let mut session = if cli.no_session {
            None
        } else if let Some(path) = &cli.session {
            Some(SessionManager::open(path).await?)
        } else if pick_session {
            // Empty shell until user picks via /resume float.
            None
        } else if cli.r#continue || (cli.resume && !pick_session) {
            // `-c` always most-recent; non-interactive `-r` same.
            match SessionManager::continue_recent(&cwd).await {
                Ok(session) => Some(session),
                Err(_) => {
                    if matches!(cli.mode, crate::cli::RunMode::Interactive) {
                        Some(SessionManager::create(&cwd).await?)
                    } else {
                        None
                    }
                }
            }
        } else if matches!(
            cli.mode,
            crate::cli::RunMode::Interactive | crate::cli::RunMode::Acp
        ) {
            // Interactive TUI and ACP both get a durable session by default
            // (ACP `session/load` / `session/list` need real session files).
            Some(SessionManager::create(&cwd).await?)
        } else {
            None
        };

        // Default thinking from settings before session override.
        if let Some(level) = user_settings
            .thinking
            .as_deref()
            .and_then(ThinkingLevel::parse)
        {
            agent.config.thinking_level = level;
        }

        if let Some(session) = &session {
            session.load_messages_into(&mut agent.messages);
            // Restore thinking level from session if present.
            if let Some(level) = session.build_session_context().thinking_level {
                if let Some(tl) = ThinkingLevel::parse(&level) {
                    agent.config.thinking_level = tl;
                }
            }
            // Restore cumulative usage for UI (does not re-enter LLM context).
            if let Some(total) = session.latest_usage_total() {
                if agent.token_usage.is_zero() {
                    agent.token_usage = total;
                }
            }
            load_extension_state(extensions.as_ref(), session);
            // Group multi-turn runs under one Langfuse session.
            agent.set_trace_session_id(Some(session.header().id.clone()));
        }
        if let (Some(session), Some(name)) = (&mut session, &cli.name) {
            session.append_session_info(name).await?;
            let _ = session.write_summary();
        }

        let steering_queue = agent.steering_queue_handle();
        let followup_queue = agent.followup_queue_handle();
        let abort_flag = agent.abort_handle();

        let intent_graph = Arc::new(tokio::sync::RwLock::new(
            one_resources::IntentGraph::load_merged(&cwd, &agent_dir),
        ));

        let mut runtime = Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            abort_flag,
            steering_queue,
            followup_queue,
            session,
            extensions,
            resources,
            auto_approve,
            cwd,
            read_only: cli.read_only,
            path_policy,
            open_session_picker: pick_session,
            mode: if start_plan {
                AgentMode::Plan
            } else {
                AgentMode::Act
            },
            plan_path,
            plan_exit,
            bg_registry,
            todo_state: one_tools::TodoListState::new(),
            memory_lookups: one_tools::MemoryLookupBudget::new(memory_opts.max_lookups_per_turn),
            env_context,
            memory_catalog,
            base_system_prompt,
            permission_gate,
            hitl,
            ask_user_handler,
            context_window: 0,
            mcp,
            mcp_tools_generation,
            mcp_reminder_state: McpReminderState::default(),
            langfuse: langfuse_sink,
            task_host,
            main_agent,
            applied_features,
            pending_features: None,
            no_subagent_process,
            no_memory_process,
            hosted_search_capable: false,
            prompt_index: 0,
            tool_audit: Vec::new(),
            tool_starts: HashMap::new(),
            intent_graph,
            intent_turn: 0,
            intent_reminder_last_turn: HashMap::new(),
        };

        // Seed session id for task parent metadata.
        runtime.sync_task_session().await;
        // Audit system prompt once for new/resumed sessions (Custom, not LLM context).
        runtime.maybe_persist_prompt_snapshot("boot").await;
        // MCP/extension tools for child harness (tools.mcp / allow MCP names).
        runtime.refresh_task_dynamic_tools().await;
        // Materialize Act tools from main AgentSpec.tools (not a fixed coding bag).
        if !start_plan {
            if let Err(e) = runtime.rebuild_act_tools().await {
                tracing::warn!(error = %e, "main ToolsSpec materialize failed; using bootstrap tools");
            }
        }

        // Restore plan path + mode from session custom entry if present.
        if !cli.read_only {
            if let Some(path) = runtime.restore_plan_path_from_session() {
                runtime.plan_path = Some(path);
            }
            if !start_plan {
                if let Some(restored) = runtime.restore_mode_from_session() {
                    if restored == AgentMode::Plan {
                        let _ = runtime.enter_plan_mode().await;
                    }
                }
            }
        }
        if start_plan {
            let _ = runtime.persist_mode().await;
        }

        Ok(runtime)
    }
}

/// Whether Langfuse tracing was requested (`--trace` / `ONE_TRACE=1`).
fn trace_enabled(cli: &Cli) -> bool {
    if cli.trace {
        return true;
    }
    std::env::var_os("ONE_TRACE").is_some_and(|v| {
        let s = v.to_string_lossy();
        s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
    })
}
