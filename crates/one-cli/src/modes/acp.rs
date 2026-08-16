//! Agent Client Protocol (ACP) server mode.
//!
//! JSON-RPC 2.0 over stdio so IDEs can drive One as a coding agent. Implements
//! [`agent_client_protocol::Agent`] and streams `session/update` for text,
//! thinking, and tool calls.
//!
//! ```text
//! one acp --cwd /project --provider xai
//! one --mode acp
//! ```
//!
//! Stdout is reserved for ACP frames; tracing stays on stderr / log files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agent_client_protocol::{
    Agent as AcpAgentTrait, AgentCapabilities, AgentSideConnection, AuthenticateRequest,
    AuthenticateResponse, AvailableCommand, AvailableCommandsUpdate, CancelNotification,
    Client as AcpClient, ClientCapabilities, ContentBlock, ContentChunk, CurrentModeUpdate,
    EmbeddedResourceResource, Error, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse,
    SelectedPermissionOutcome, SessionCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionMode, SessionModeId, SessionModeState, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
    SetSessionModelRequest, SetSessionModelResponse, StopReason, ToolCall as AcpToolCall,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use async_trait::async_trait;
use one_core::agent::ThinkingLevel;
use one_core::events::AgentEvent;
use one_core::message::{AgentMessage, TextOrImage, UserContent};
use one_core::tool::{ToolCall as CoreToolCall, ToolOutput};
use one_session::SessionManager;
use one_tui::SelectResult;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::approval::{ApprovalChoice, ApprovalRequest, PermissionGate};
use crate::cli::Cli;
use crate::hitl::HitlChannel;
use crate::provider::ProviderSet;
use crate::runtime::{AgentMode, AppRuntime};

// ── Entry ────────────────────────────────────────────────────────────────────

/// Run the ACP stdio server until the client disconnects.
pub async fn run_acp(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let agent = Rc::new(OneAcpAgent::new(cli));
            let outgoing = tokio::io::stdout().compat_write();
            let incoming = tokio::io::stdin().compat();

            let (conn, io_task) =
                AgentSideConnection::new(agent.clone(), outgoing, incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });
            agent.set_client(Rc::new(conn));

            if let Err(err) = io_task.await {
                tracing::debug!(error = %err, "acp io task ended");
            }
            agent.shutdown().await;
            Ok(())
        })
        .await
}

// ── Types ────────────────────────────────────────────────────────────────────

struct OneAcpAgent {
    cli: StdMutex<Cli>,
    client: StdMutex<Option<Rc<AgentSideConnection>>>,
    sessions: tokio::sync::Mutex<HashMap<SessionId, Arc<SessionHandle>>>,
    #[allow(dead_code)]
    client_caps: StdMutex<ClientCapabilities>,
}

/// Per-session state. Map holds `Arc` so cancel / prompt can proceed without
/// holding the sessions map lock across the LLM turn.
struct SessionHandle {
    runtime: tokio::sync::Mutex<AppRuntime>,
    provider: tokio::sync::Mutex<ProviderSet>,
    cwd: PathBuf,
    /// Process-level cancel for this prompt turn (also sets runtime abort).
    cancel: Arc<AtomicBool>,
    /// Runtime abort flag (shared with Agent loop).
    abort: Arc<AtomicBool>,
    busy: AtomicBool,
    permission_gate: Arc<PermissionGate>,
    hitl: HitlChannel,
}

impl OneAcpAgent {
    fn new(cli: Cli) -> Self {
        Self {
            cli: StdMutex::new(cli),
            client: StdMutex::new(None),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            client_caps: StdMutex::new(ClientCapabilities::default()),
        }
    }

    fn set_client(&self, conn: Rc<AgentSideConnection>) {
        *self.client.lock().expect("client lock") = Some(conn);
    }

    fn client(&self) -> Result<Rc<AgentSideConnection>, Error> {
        self.client
            .lock()
            .expect("client lock")
            .clone()
            .ok_or_else(|| err_internal("acp client not ready"))
    }

    async fn shutdown(&self) {
        let mut sessions = self.sessions.lock().await;
        for (_, handle) in sessions.drain() {
            let rt = handle.runtime.lock().await;
            rt.shutdown_owned_tasks();
            rt.flush_trace();
        }
    }

    fn mode_state(current: AgentMode) -> SessionModeState {
        let modes = vec![
            SessionMode::new(SessionModeId::new("act"), "Act").description(
                "Full coding tools: read, edit, bash, MCP, subagents.",
            ),
            SessionMode::new(SessionModeId::new("plan"), "Plan").description(
                "Read-only exploration + plan file; no code edits until Act.",
            ),
        ];
        let id = match current {
            AgentMode::Act => SessionModeId::new("act"),
            AgentMode::Plan => SessionModeId::new("plan"),
        };
        SessionModeState::new(id, modes)
    }

    fn available_commands() -> AvailableCommandsUpdate {
        AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("plan", "Switch to Plan mode (explore + write a plan)"),
            AvailableCommand::new("act", "Switch to Act/Build mode (full coding tools)"),
            AvailableCommand::new("compact", "Compact conversation context"),
            AvailableCommand::new(
                "thinking",
                "Set thinking level (off|low|medium|high). Usage: /thinking medium",
            ),
        ])
    }

    async fn notify(&self, session_id: &SessionId, update: SessionUpdate) {
        let Ok(client) = self.client() else {
            return;
        };
        let n = SessionNotification::new(session_id.clone(), update);
        if let Err(err) = client.session_notification(n).await {
            tracing::debug!(error = %err, "acp session_notification failed");
        }
    }

    async fn build_handle(
        &self,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
    ) -> Result<(SessionId, Arc<SessionHandle>), Error> {
        let mut cli = self.cli.lock().expect("cli lock").clone();
        cli.mode = crate::cli::RunMode::Acp;
        cli.cwd = cwd.clone();
        cli.print = None;
        if let Some(path) = session_path {
            cli.session = Some(path);
            cli.r#continue = false;
            cli.resume = false;
            cli.no_session = false;
        } else {
            cli.session = None;
            cli.r#continue = false;
            cli.resume = false;
        }

        // Handlers run on a LocalSet (`agent-client-protocol` is `?Send`).
        // Auth resolve is LocalSet-safe (dedicated OS thread); build in place.
        build_session_components(cli, cwd)
            .await
            .map_err(err_internal)
    }

    async fn get_session(&self, id: &SessionId) -> Result<Arc<SessionHandle>, Error> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| err_params("unknown session"))
    }

    async fn replay_history(&self, session_id: &SessionId, handle: &SessionHandle) {
        let messages = {
            let rt = handle.runtime.lock().await;
            let agent = rt.agent.lock().await;
            agent.messages.clone()
        };
        for msg in messages {
            match msg {
                AgentMessage::User(u) => {
                    let text = user_content_text(&u.content);
                    if !text.is_empty() {
                        self.notify(
                            session_id,
                            SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::from(
                                text,
                            ))),
                        )
                        .await;
                    }
                }
                AgentMessage::Assistant(a) => {
                    for block in &a.content {
                        match block {
                            one_core::message::ContentBlock::Text { text } if !text.is_empty() => {
                                self.notify(
                                    session_id,
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::from(text.clone()),
                                    )),
                                )
                                .await;
                            }
                            one_core::message::ContentBlock::Thinking { thinking, .. }
                                if !thinking.is_empty() =>
                            {
                                self.notify(
                                    session_id,
                                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                                        ContentBlock::from(thinking.clone()),
                                    )),
                                )
                                .await;
                            }
                            one_core::message::ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => {
                                let call = AcpToolCall::new(ToolCallId::new(id.as_str()), name.clone())
                                    .kind(tool_kind(name))
                                    .status(ToolCallStatus::Completed)
                                    .raw_input(arguments.clone());
                                self.notify(session_id, SessionUpdate::ToolCall(call)).await;
                            }
                            _ => {}
                        }
                    }
                }
                AgentMessage::ToolResult(tr) => {
                    let text = tool_result_text(&tr.content);
                    let update = ToolCallUpdate::new(
                        ToolCallId::new(tr.tool_call_id.as_str()),
                        ToolCallUpdateFields::new()
                            .status(if tr.is_error {
                                ToolCallStatus::Failed
                            } else {
                                ToolCallStatus::Completed
                            })
                            .content(vec![ToolCallContent::from(text)])
                            .raw_output(serde_json::json!({ "is_error": tr.is_error })),
                    );
                    self.notify(session_id, SessionUpdate::ToolCallUpdate(update))
                        .await;
                }
            }
        }
    }
}

// ── Agent trait ──────────────────────────────────────────────────────────────

#[async_trait(?Send)]
impl AcpAgentTrait for OneAcpAgent {
    async fn initialize(
        &self,
        args: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        *self.client_caps.lock().expect("caps") = args.client_capabilities.clone();

        let mut session_caps = SessionCapabilities::default();
        session_caps.list = Some(SessionListCapabilities::default());
        session_caps.resume = Some(SessionResumeCapabilities::default());

        let caps = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(true)
                    .embedded_context(true),
            )
            .mcp_capabilities(McpCapabilities::default())
            .session_capabilities(session_caps);

        Ok(InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(caps)
            .agent_info(
                Implementation::new("one", env!("CARGO_PKG_VERSION")).title("One coding agent"),
            )
            .auth_methods(vec![]))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        args: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        if !args.mcp_servers.is_empty() {
            tracing::info!(
                count = args.mcp_servers.len(),
                "acp session/new: client MCP servers ignored; configure via ~/.one/agent/mcp.json"
            );
        }

        let cwd = canonicalize_cwd(&args.cwd);
        let (session_id, handle) = self.build_handle(cwd, None).await?;
        let mode = {
            let rt = handle.runtime.lock().await;
            rt.mode()
        };
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), handle);

        self.notify(
            &session_id,
            SessionUpdate::AvailableCommandsUpdate(Self::available_commands()),
        )
        .await;

        Ok(NewSessionResponse::new(session_id).modes(Self::mode_state(mode)))
    }

    async fn load_session(
        &self,
        args: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        let cwd = canonicalize_cwd(&args.cwd);
        let sid = args.session_id.clone();

        if let Ok(existing) = self.get_session(&sid).await {
            let mode = existing.runtime.lock().await.mode();
            self.replay_history(&sid, &existing).await;
            return Ok(LoadSessionResponse::new().modes(Self::mode_state(mode)));
        }

        let path = resolve_session_path(&cwd, sid.0.as_ref())
            .await
            .map_err(err_params)?;
        let (session_id, handle) = self.build_handle(cwd, Some(path)).await?;
        let mode = handle.runtime.lock().await.mode();
        self.replay_history(&session_id, &handle).await;
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), handle);

        self.notify(
            &session_id,
            SessionUpdate::AvailableCommandsUpdate(Self::available_commands()),
        )
        .await;

        Ok(LoadSessionResponse::new().modes(Self::mode_state(mode)))
    }

    async fn resume_session(
        &self,
        args: ResumeSessionRequest,
    ) -> agent_client_protocol::Result<ResumeSessionResponse> {
        let cwd = canonicalize_cwd(&args.cwd);
        let sid = args.session_id.clone();
        if self.sessions.lock().await.contains_key(&sid) {
            return Ok(ResumeSessionResponse::new());
        }
        let path = resolve_session_path(&cwd, sid.0.as_ref())
            .await
            .map_err(err_params)?;
        let (session_id, handle) = self.build_handle(cwd, Some(path)).await?;
        self.sessions.lock().await.insert(session_id, handle);
        Ok(ResumeSessionResponse::new())
    }

    async fn list_sessions(
        &self,
        args: ListSessionsRequest,
    ) -> agent_client_protocol::Result<ListSessionsResponse> {
        let cwd = match args.cwd.as_ref() {
            Some(p) => canonicalize_cwd(p),
            None => {
                let cli = self.cli.lock().expect("cli");
                cli.cwd.canonicalize().unwrap_or_else(|_| cli.cwd.clone())
            }
        };

        let listed = SessionManager::list(&cwd)
            .await
            .map_err(|e| err_internal(e.to_string()))?;

        let sessions = listed
            .into_iter()
            .take(50)
            .map(|s| {
                let mut info = SessionInfo::new(SessionId::new(s.id.as_str()), cwd.clone());
                let title = s.display_label();
                if !title.is_empty() {
                    info = info.title(title);
                }
                info = info.updated_at(s.modified.to_rfc3339());
                info
            })
            .collect();

        Ok(ListSessionsResponse::new(sessions))
    }

    async fn set_session_mode(
        &self,
        args: SetSessionModeRequest,
    ) -> agent_client_protocol::Result<SetSessionModeResponse> {
        let handle = self.get_session(&args.session_id).await?;
        let mut rt = handle.runtime.lock().await;
        match args.mode_id.0.as_ref() {
            "plan" => {
                rt.enter_plan_mode()
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
            }
            "act" | "build" | "agent" => {
                rt.leave_plan_mode()
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
            }
            other => return Err(err_params(format!("unknown mode: {other}"))),
        }
        drop(rt);

        self.notify(
            &args.session_id,
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(args.mode_id.clone())),
        )
        .await;
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_model(
        &self,
        args: SetSessionModelRequest,
    ) -> agent_client_protocol::Result<SetSessionModelResponse> {
        let handle = self.get_session(&args.session_id).await?;
        let model_id = args.model_id.0.to_string();
        let mut cli = self.cli.lock().expect("cli").clone();
        cli.model = Some(model_id);
        let set = ProviderSet::build(&cli)
            .map_err(|e| err_params(format!("model switch: {e}")))?;
        {
            let mut rt = handle.runtime.lock().await;
            rt.set_context_window(set.context_window());
            let _ = rt.refresh_web_search_backend(&set).await;
            rt.bind_task_provider(set.as_arc()).await;
        }
        *handle.provider.lock().await = set;
        Ok(SetSessionModelResponse::default())
    }

    async fn set_session_config_option(
        &self,
        args: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        let handle = self.get_session(&args.session_id).await?;
        if args.config_id.0.as_ref() == "thinking" {
            let level = ThinkingLevel::parse(args.value.0.as_ref())
                .ok_or_else(|| err_params("thinking: off|low|medium|high"))?;
            handle
                .runtime
                .lock()
                .await
                .set_thinking_level(level)
                .await
                .map_err(|e| err_internal(e.to_string()))?;
            Ok(SetSessionConfigOptionResponse::new(vec![]))
        } else {
            Err(err_params(format!(
                "unknown config option: {}",
                args.config_id.0
            )))
        }
    }

    async fn prompt(&self, args: PromptRequest) -> agent_client_protocol::Result<PromptResponse> {
        let session_id = args.session_id.clone();
        let prompt_text = content_blocks_to_text(&args.prompt);
        if prompt_text.trim().is_empty() {
            return Err(err_params("empty prompt"));
        }

        if let Some(resp) = self
            .try_slash_command(&session_id, prompt_text.trim())
            .await?
        {
            return Ok(resp);
        }

        let handle = self.get_session(&session_id).await?;
        if handle
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(err_invalid("session already processing a prompt"));
        }

        handle.cancel.store(false, Ordering::SeqCst);
        handle.abort.store(false, Ordering::SeqCst);
        {
            let rt = handle.runtime.lock().await;
            rt.clear_abort();
        }

        let client = self.client()?;

        // Event bridge
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        {
            let rt = handle.runtime.lock().await;
            let mut agent = rt.agent.lock().await;
            agent.clear_listeners();
            agent.subscribe(Box::new(move |ev: &AgentEvent| {
                let _ = ev_tx.send(ev.clone());
            }));
        }

        let sid_ev = session_id.clone();
        let client_ev = client.clone();
        let event_task = tokio::task::spawn_local(async move {
            while let Some(ev) = ev_rx.recv().await {
                if let Some(update) = event_to_update(&ev) {
                    let n = SessionNotification::new(sid_ev.clone(), update);
                    let _ = client_ev.session_notification(n).await;
                }
            }
        });

        // Approval bridge
        let sid_ap = session_id.clone();
        let client_ap = client.clone();
        let cancel_ap = handle.cancel.clone();
        let gate = handle.permission_gate.clone();
        let hitl = handle.hitl.clone();
        let approval_task = tokio::task::spawn_local(async move {
            loop {
                if cancel_ap.load(Ordering::SeqCst) {
                    gate.cancel_pending();
                    hitl.cancel_pending();
                    break;
                }
                if let Some(req) = gate.poll_request() {
                    let mapped = request_tool_permission(&client_ap, &sid_ap, &req).await;
                    gate.respond(mapped);
                }
                if let Some(req) = hitl.poll_request() {
                    let result = request_hitl(&client_ap, &sid_ap, &req).await;
                    hitl.respond(result);
                }
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
        });

        // Run prompt
        let result = {
            let mut rt = handle.runtime.lock().await;
            let provider = handle.provider.lock().await;
            rt.prompt(provider.as_llm(), &prompt_text).await
        };

        // Tear down bridges: clear listeners (drops ev_tx), stop approval loop.
        {
            let rt = handle.runtime.lock().await;
            let mut agent = rt.agent.lock().await;
            agent.clear_listeners();
        }
        handle.cancel.store(true, Ordering::SeqCst);
        let _ = event_task.await;
        approval_task.abort();

        let was_cancelled = handle.abort.load(Ordering::SeqCst);
        handle.busy.store(false, Ordering::SeqCst);
        handle.cancel.store(false, Ordering::SeqCst);

        match result {
            Ok(_) if was_cancelled => Ok(PromptResponse::new(StopReason::Cancelled)),
            Ok(_) => Ok(PromptResponse::new(StopReason::EndTurn)),
            Err(e) => {
                let msg = e.to_string();
                let low = msg.to_ascii_lowercase();
                if was_cancelled || low.contains("cancel") || low.contains("abort") {
                    Ok(PromptResponse::new(StopReason::Cancelled))
                } else if low.contains("max turn") || low.contains("max_turns") {
                    Ok(PromptResponse::new(StopReason::MaxTurnRequests))
                } else {
                    self.notify(
                        &session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                            format!("Error: {msg}"),
                        ))),
                    )
                    .await;
                    Ok(PromptResponse::new(StopReason::EndTurn))
                }
            }
        }
    }

    async fn cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        if let Ok(handle) = self.get_session(&args.session_id).await {
            handle.cancel.store(true, Ordering::SeqCst);
            handle.abort.store(true, Ordering::SeqCst);
            // Best-effort: also kill subagent jobs if we can take the lock quickly.
            if let Ok(rt) = handle.runtime.try_lock() {
                rt.abort();
                rt.permission_gate.cancel_pending();
                rt.hitl.cancel_pending();
            } else {
                handle.permission_gate.cancel_pending();
                handle.hitl.cancel_pending();
            }
        }
        Ok(())
    }
}

impl OneAcpAgent {
    async fn try_slash_command(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> agent_client_protocol::Result<Option<PromptResponse>> {
        let trimmed = text.trim();
        if !trimmed.starts_with('/') {
            return Ok(None);
        }
        let (cmd, rest) = match trimmed[1..].split_once(char::is_whitespace) {
            Some((c, r)) => (c.to_ascii_lowercase(), r.trim()),
            None => (trimmed[1..].to_ascii_lowercase(), ""),
        };

        let handle = match self.get_session(session_id).await {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };

        match cmd.as_str() {
            "plan" => {
                handle
                    .runtime
                    .lock()
                    .await
                    .enter_plan_mode()
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
                self.notify(
                    session_id,
                    SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(
                        "plan",
                    ))),
                )
                .await;
                self.notify(
                    session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        "Switched to Plan mode.",
                    ))),
                )
                .await;
                Ok(Some(PromptResponse::new(StopReason::EndTurn)))
            }
            "act" | "build" => {
                handle
                    .runtime
                    .lock()
                    .await
                    .leave_plan_mode()
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
                self.notify(
                    session_id,
                    SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(
                        "act",
                    ))),
                )
                .await;
                self.notify(
                    session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        "Switched to Act mode.",
                    ))),
                )
                .await;
                Ok(Some(PromptResponse::new(StopReason::EndTurn)))
            }
            "compact" => {
                let mut rt = handle.runtime.lock().await;
                let provider = handle.provider.lock().await;
                rt.maybe_compact(provider.as_llm(), true)
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
                drop(provider);
                drop(rt);
                self.notify(
                    session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        "Context compacted.",
                    ))),
                )
                .await;
                Ok(Some(PromptResponse::new(StopReason::EndTurn)))
            }
            "thinking" if !rest.is_empty() => {
                let level = ThinkingLevel::parse(rest)
                    .ok_or_else(|| err_params("usage: /thinking off|low|medium|high"))?;
                handle
                    .runtime
                    .lock()
                    .await
                    .set_thinking_level(level)
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
                self.notify(
                    session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        format!("Thinking level set to {}.", level.as_str()),
                    ))),
                )
                .await;
                Ok(Some(PromptResponse::new(StopReason::EndTurn)))
            }
            _ => Ok(None),
        }
    }
}

// ── Session assembly (Send; multi-thread runtime) ────────────────────────────

async fn build_session_components(
    cli: Cli,
    cwd: PathBuf,
) -> Result<(SessionId, Arc<SessionHandle>), String> {
    let mut providers =
        ProviderSet::build(&cli).map_err(|e| format!("provider: {e}"))?;
    let mut runtime = AppRuntime::build(&cli)
        .await
        .map_err(|e| format!("runtime: {e}"))?;

    if cli.provider.is_none() && cli.model.is_none() {
        if let Some((provider, model)) = runtime.session.as_ref().and_then(|session| {
            let context = session.build_session_context();
            context.provider.zip(context.model_id)
        }) {
            let _ = providers.restore_session_model(&provider, &model);
        }
    }
    runtime.set_context_window(providers.context_window());
    let _ = runtime.refresh_web_search_backend(&providers).await;
    runtime.bind_task_provider(providers.as_arc()).await;
    runtime.sync_task_session().await;

    let session_id = if let Some(s) = runtime.session.as_ref() {
        SessionId::new(s.header().id.as_str())
    } else {
        SessionId::new(format!("ephemeral-{}", uuid::Uuid::new_v4()))
    };

    let abort = runtime.abort_handle();
    let permission_gate = runtime.permission_gate.clone();
    let hitl = runtime.hitl.clone();

    let handle = Arc::new(SessionHandle {
        runtime: tokio::sync::Mutex::new(runtime),
        provider: tokio::sync::Mutex::new(providers),
        cwd,
        cancel: Arc::new(AtomicBool::new(false)),
        abort,
        busy: AtomicBool::new(false),
        permission_gate,
        hitl,
    });
    Ok((session_id, handle))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn err_internal(msg: impl Into<String>) -> Error {
    Error::internal_error().data(serde_json::Value::String(msg.into()))
}

fn err_params(msg: impl Into<String>) -> Error {
    Error::invalid_params().data(serde_json::Value::String(msg.into()))
}

fn err_invalid(msg: impl Into<String>) -> Error {
    Error::invalid_request().data(serde_json::Value::String(msg.into()))
}

async fn request_tool_permission(
    client: &AgentSideConnection,
    session_id: &SessionId,
    req: &ApprovalRequest,
) -> ApprovalChoice {
    let options = vec![
        PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow-session",
            "Allow for session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            "allow-always",
            "Always allow (this process)",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
    ];
    let title = if req.summary.is_empty() {
        format!("{} — {}", req.tool, req.reason)
    } else {
        req.summary.clone()
    };
    let tool_update = ToolCallUpdate::new(
        ToolCallId::new(format!("perm-{}", req.id)),
        ToolCallUpdateFields::new()
            .title(title)
            .kind(tool_kind(&req.tool))
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({
                "tool": req.tool,
                "reason": req.reason,
                "fingerprint": req.fingerprint,
            })),
    );
    match client
        .request_permission(RequestPermissionRequest::new(
            session_id.clone(),
            tool_update,
            options,
        ))
        .await
    {
        Ok(resp) => match resp.outcome {
            RequestPermissionOutcome::Cancelled => ApprovalChoice::Deny {
                feedback: Some("cancelled".into()),
            },
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                match option_id.0.as_ref() {
                    "allow-once" => ApprovalChoice::Once,
                    "allow-session" => ApprovalChoice::Session,
                    "allow-always" => ApprovalChoice::Always,
                    _ => ApprovalChoice::Deny { feedback: None },
                }
            }
            _ => ApprovalChoice::Deny { feedback: None },
        },
        Err(_) => ApprovalChoice::Deny {
            feedback: Some("permission request failed".into()),
        },
    }
}

async fn request_hitl(
    client: &AgentSideConnection,
    session_id: &SessionId,
    req: &crate::hitl::HitlSelectRequest,
) -> SelectResult {
    let options: Vec<PermissionOption> = req
        .prompt
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            PermissionOption::new(
                o.id.clone(),
                o.label.clone(),
                if i == 0 {
                    PermissionOptionKind::AllowOnce
                } else {
                    PermissionOptionKind::RejectOnce
                },
            )
        })
        .collect();
    let tool_update = ToolCallUpdate::new(
        ToolCallId::new(format!("ask-{}", req.id)),
        ToolCallUpdateFields::new()
            .title(req.prompt.title.clone())
            .kind(ToolKind::Other)
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({ "body": req.prompt.body })),
    );
    match client
        .request_permission(RequestPermissionRequest::new(
            session_id.clone(),
            tool_update,
            options,
        ))
        .await
    {
        Ok(resp) => match resp.outcome {
            RequestPermissionOutcome::Cancelled => SelectResult::Cancelled,
            RequestPermissionOutcome::Selected(sel) => SelectResult::Confirmed {
                ids: vec![sel.option_id.0.to_string()],
                other: None,
            },
            _ => SelectResult::Cancelled,
        },
        Err(_) => SelectResult::Cancelled,
    }
}

fn event_to_update(ev: &AgentEvent) -> Option<SessionUpdate> {
    match ev {
        AgentEvent::TextDelta { delta } if !delta.is_empty() => Some(
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(delta.clone()))),
        ),
        AgentEvent::ThinkingDelta { delta } if !delta.is_empty() => Some(
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from(delta.clone()))),
        ),
        AgentEvent::ToolExecutionStart { tool_call } => {
            let call = AcpToolCall::new(
                ToolCallId::new(tool_call.id.as_str()),
                tool_title(tool_call),
            )
            .kind(tool_kind(&tool_call.name))
            .status(ToolCallStatus::InProgress)
            .locations(tool_locations(tool_call))
            .raw_input(tool_call.arguments.clone());
            Some(SessionUpdate::ToolCall(call))
        }
        AgentEvent::ToolExecutionEnd {
            tool_call,
            output,
            is_error,
        } => {
            let status = if *is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let update = ToolCallUpdate::new(
                ToolCallId::new(tool_call.id.as_str()),
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![ToolCallContent::from(truncate_output(output))])
                    .raw_output(serde_json::json!({ "is_error": is_error })),
            );
            Some(SessionUpdate::ToolCallUpdate(update))
        }
        _ => None,
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "ls" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "grep" | "find" | "memory_search" | "search_tool" => ToolKind::Search,
        "bash" | "bash_output" | "bash_kill" | "monitor" => ToolKind::Execute,
        "web_search" | "web_fetch" | "use_tool" => ToolKind::Fetch,
        "plan" | "exit_plan_mode" | "todo_write" => ToolKind::Think,
        "task" | "job_output" | "wait_tasks" | "job_kill" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}

fn tool_title(call: &CoreToolCall) -> String {
    let args = &call.arguments;
    match call.name.as_str() {
        "read" | "write" | "edit" => {
            let path = args
                .get("path")
                .or_else(|| args.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("{} {path}", call.name)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("…");
            let short: String = cmd.chars().take(80).collect();
            format!("bash {short}")
        }
        "grep" => {
            let pat = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            format!("grep {pat}")
        }
        other => other.to_string(),
    }
}

fn tool_locations(call: &CoreToolCall) -> Vec<ToolCallLocation> {
    match call
        .arguments
        .get("path")
        .or_else(|| call.arguments.get("file_path"))
        .and_then(|v| v.as_str())
    {
        Some(p) if !p.is_empty() => vec![ToolCallLocation::new(p)],
        _ => vec![],
    }
}

fn truncate_output(output: &ToolOutput) -> String {
    let text = output.as_text();
    const MAX: usize = 8_000;
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!(
            "{}…\n\n[truncated {} bytes]",
            &text[..MAX],
            text.len().saturating_sub(MAX)
        )
    }
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[resource: {}]({})", link.name, link.uri));
            }
            ContentBlock::Resource(res) => match &res.resource {
                EmbeddedResourceResource::TextResourceContents(t) => {
                    parts.push(format!("### {}\n{}", t.uri, t.text));
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    parts.push(format!(
                        "[binary resource {} · {} bytes base64]",
                        blob.uri,
                        blob.blob.len()
                    ));
                }
                _ => {}
            },
            ContentBlock::Image(img) => {
                parts.push(format!(
                    "[image {} · {} bytes base64]",
                    img.mime_type,
                    img.data.len()
                ));
            }
            ContentBlock::Audio(a) => {
                parts.push(format!(
                    "[audio {} · {} bytes base64]",
                    a.mime_type,
                    a.data.len()
                ));
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(t) => t.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                TextOrImage::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn tool_result_text(content: &[TextOrImage]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            TextOrImage::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn canonicalize_cwd(cwd: &Path) -> PathBuf {
    cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())
}

async fn resolve_session_path(cwd: &Path, spec: &str) -> Result<PathBuf, String> {
    let as_path = PathBuf::from(spec);
    if as_path.is_file() {
        return Ok(as_path);
    }
    match SessionManager::resolve(cwd, spec).await {
        Ok(info) => Ok(info.path),
        Err(e) => Err(e.to_string()),
    }
}
