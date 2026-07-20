use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::error::{OneError, Result};
use crate::events::{AgentEvent, EventListener};
use crate::hooks::AgentHooks;
use crate::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage, now_ms,
};
use crate::tool::{Tool, ToolCall, ToolOutput};
use crate::tool_gate::{ToolGate, ToolGateDecision};
use crate::trace::{
    args_preview, new_run_id, SharedTrace, TraceEvent, TraceGateDecision, TraceRunStatus,
};

/// Core role + tool policy for the coding agent.
///
/// Feature packages (subagent/task, …) are **not** included here — the CLI
/// prompt composer attaches them when the matching settings feature is enabled.
/// Keep this string free of optional capability prose so disabled features do
/// not leak into the model context.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an AI coding assistant. Use the provided tools to read and change files, run shell commands, and search or fetch the web when you need current information.

Tool choice (prefer specialized tools over bash — Claude Code style):
- Explore: `ls`, `find` (glob), `grep`, `read` — not `bash` with find/rg/cat/head/sed/awk pipelines.
- Edit: `edit` / `write` — not shell redirection or sed/awk rewrites.
- Run: `bash` only for real process work (build, test, git, package managers, long-running commands). Always pass a short `description` for bash commands.
- Never use bash echo (or similar) to talk to the user; reply in normal assistant text.
- Do not assume host extras exist (`rg`, `tree`, `eza`, `fd`, …). The `grep` tool uses ripgrep when available; if a tool fails with missing binary, fall back to another tool or plain `grep`/`find` via bash only when needed.
- You may request multiple independent tool calls in one turn; read-only tools (read/grep/find/ls/…) may run concurrently, while write/edit/bash and similar run serially.
- Path args accept `path` or Claude-style `file_path`.

File changes:
- Prefer `edit` for localized fixes. By default `old_string` must uniquely match once; set `replace_all=true` to replace every occurrence (e.g. renames).
- Use `write` only for new files or intentional full-file rewrites — do not rewrite an entire file when a small edit would do.
- Read a file before editing it when you need its current contents.

Search:
- Prefer `grep` with `glob` / `type` / `output_mode` / `head_limit` / context (`-A`/`-B`/`-C`) over shell ripgrep.

Bash / sandbox:
- Default bash runs under an OS sandbox (workspace-write): workspace is writable; home and system paths are mostly read-only. Prefer the dedicated file tools so path policy and truncation stay consistent.
- Keep commands focused; avoid huge recursive dumps. If output is truncated or spilled to a file, read the spill instead of re-running wider.

When requirements are ambiguous, use the `ask_user` tool instead of guessing. Be concise and precise. Do not load unrelated skills or docs unless they clearly help the task.";

/// Reasoning / extended-thinking intensity (provider-specific mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" => Some(ThinkingLevel::Off),
            "low" | "1" | "minimal" => Some(ThinkingLevel::Low),
            "medium" | "med" | "2" => Some(ThinkingLevel::Medium),
            "high" | "3" | "xhigh" | "max" => Some(ThinkingLevel::High),
            _ => None,
        }
    }

    pub fn cycle_next(self) -> Self {
        match self {
            ThinkingLevel::Off => ThinkingLevel::Low,
            ThinkingLevel::Low => ThinkingLevel::Medium,
            ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::Off,
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, ThinkingLevel::Off)
    }

    /// OpenAI / OpenRouter style effort label (`None` when off).
    pub fn effort(self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("low"),
            ThinkingLevel::Medium => Some("medium"),
            ThinkingLevel::High => Some("high"),
        }
    }

    /// Anthropic-style token budget for extended thinking (`None` when off).
    ///
    /// Defaults align with Pi's budgets (low 2k / medium 8k / high 16k).
    pub fn budget_tokens(self) -> Option<u32> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some(2_048),
            ThinkingLevel::Medium => Some(8_192),
            ThinkingLevel::High => Some(16_384),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    pub max_turns: usize,
    pub thinking_level: ThinkingLevel,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_turns: 32,
            thinking_level: ThinkingLevel::Off,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<crate::tool::ToolDefinition>,
    pub thinking_level: ThinkingLevel,
}

/// Token accounting returned by providers (when available).
///
/// Field semantics (important for cost / totals):
/// - **Anthropic**: `input_tokens` excludes cache; `cache_read` / `cache_write` are disjoint.
/// - **OpenAI**: `input_tokens` (`prompt_tokens`) **includes** `cache_read_tokens` as a subset.
/// - `total()` is therefore **input + output only** (never double-counts OpenAI cache).
/// - Use [`prompt_tokens_expanded`] for Anthropic-style full prompt size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    /// Input + output as reported (OpenAI-safe; no cache double-count).
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Anthropic-style expanded prompt size: input + cache_read + cache_write.
    ///
    /// Do **not** use for OpenAI (where `cache_read` is already inside `input_tokens`).
    pub fn prompt_tokens_expanded(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Non-cached input tokens when `cache_read` is a **subset** of `input` (OpenAI).
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cache_read_tokens)
    }

    pub fn add_assign(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }

    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
    }

    /// Best-effort size of the **prompt/context** for this completion (for compaction).
    ///
    /// Anthropic reports cache fields disjoint from `input_tokens`; OpenAI folds
    /// cache hits into `input_tokens`. Prefer expanded size when write-cache is set.
    pub fn context_size_tokens(&self) -> u64 {
        if self.is_zero() {
            return 0;
        }
        if self.cache_write_tokens > 0 {
            self.prompt_tokens_expanded()
        } else {
            self.input_tokens
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub provider: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    /// Provider-reported usage for this completion (may be zero if unknown).
    pub usage: TokenUsage,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_event: &mut (dyn FnMut(crate::streaming::StreamEvent) + Send),
        abort: Option<&AtomicBool>,
    ) -> Result<CompletionResponse> {
        let response = self.complete(request).await?;
        let text = extract_text(&response.content);
        if !text.is_empty() {
            crate::streaming::emit_text_chunks(&text, 8, on_event, abort);
        }
        if abort.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let mut partial = response;
            partial.stop_reason = StopReason::Aborted;
            return Ok(partial);
        }
        Ok(response)
    }
}

pub struct Agent {
    pub config: AgentConfig,
    pub messages: Vec<AgentMessage>,
    pub is_busy: bool,
    /// Cumulative provider-reported tokens for this process/session.
    pub token_usage: TokenUsage,
    /// Last completion's prompt/context size (not cumulative). 0 if unknown.
    /// Used by compaction to prefer API usage over char/4 estimates.
    pub last_prompt_tokens: u64,
    tools: Vec<Arc<dyn Tool>>,
    listeners: Vec<EventListener>,
    steering_queue: Arc<Mutex<Vec<String>>>,
    followup_queue: Arc<Mutex<Vec<String>>>,
    /// Side-channel notices (e.g. background bash completions), drained before each LLM turn.
    /// Injected as user messages with a clear prefix — not tool_results (providers require pairing).
    notification_queue: Arc<Mutex<Vec<String>>>,
    abort_flag: Arc<AtomicBool>,
    /// Optional external turn counter (1-based completed turns) for job progress UIs.
    turn_progress: Option<Arc<AtomicU64>>,
    /// Optional pre-tool permission gate (allow/deny/ask/rewrite).
    tool_gate: Option<Arc<dyn ToolGate>>,
    /// Optional async lifecycle hooks (extensions bridge).
    hooks: Option<Arc<dyn AgentHooks>>,
    /// Optional execution trace sink (harness eval). Default: none (zero cost).
    trace: Option<SharedTrace>,
    /// Metadata for the next / current run (set by CLI/bench before `prompt`).
    trace_meta: TraceRunMeta,
}

/// Optional labels attached to the next agent run's `run_start` event.
#[derive(Debug, Clone, Default)]
pub struct TraceRunMeta {
    pub task_id: Option<String>,
    pub agent_version: Option<String>,
    pub config: Option<serde_json::Value>,
    /// Langfuse / OTEL session id (multi-turn conversation grouping).
    pub session_id: Option<String>,
    /// Optional end-user id (`langfuse.user.id`).
    pub user_id: Option<String>,
    /// When true, include larger I/O previews on LLM / tool / run events.
    pub trace_full: bool,
}

impl Agent {
    pub fn new(config: AgentConfig, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            config,
            messages: Vec::new(),
            is_busy: false,
            token_usage: TokenUsage::default(),
            last_prompt_tokens: 0,
            tools,
            listeners: Vec::new(),
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            followup_queue: Arc::new(Mutex::new(Vec::new())),
            notification_queue: Arc::new(Mutex::new(Vec::new())),
            abort_flag: Arc::new(AtomicBool::new(false)),
            turn_progress: None,
            tool_gate: None,
            hooks: None,
            trace: None,
            trace_meta: TraceRunMeta::default(),
        }
    }

    /// Install a permission gate checked before every tool execution.
    pub fn set_tool_gate(&mut self, gate: Option<Arc<dyn ToolGate>>) {
        self.tool_gate = gate;
    }

    pub fn tool_gate(&self) -> Option<&Arc<dyn ToolGate>> {
        self.tool_gate.as_ref()
    }

    /// Install async lifecycle hooks (session / turn boundaries).
    pub fn set_hooks(&mut self, hooks: Option<Arc<dyn AgentHooks>>) {
        self.hooks = hooks;
    }

    pub fn hooks(&self) -> Option<&Arc<dyn AgentHooks>> {
        self.hooks.as_ref()
    }

    /// Install an optional execution-trace sink (harness eval / `--trace`).
    ///
    /// When `None` (default), tracing is a no-op with no allocations per event.
    pub fn set_trace(&mut self, sink: Option<SharedTrace>) {
        self.trace = sink;
    }

    pub fn trace(&self) -> Option<&SharedTrace> {
        self.trace.as_ref()
    }

    /// Labels included on the next `run_start` (task id, version, config snapshot).
    pub fn set_trace_meta(&mut self, meta: TraceRunMeta) {
        self.trace_meta = meta;
    }

    pub fn trace_meta(&self) -> &TraceRunMeta {
        &self.trace_meta
    }

    /// Update session id for the next run (e.g. after `/new` or `/resume`).
    pub fn set_trace_session_id(&mut self, session_id: Option<String>) {
        self.trace_meta.session_id = session_id;
    }

    fn record_trace(&self, event: TraceEvent) {
        if let Some(sink) = &self.trace {
            sink.record(event);
        }
    }

    fn preview_limit(&self) -> usize {
        if self.trace_meta.trace_full {
            crate::trace::PREVIEW_FULL_CHARS
        } else {
            crate::trace::PREVIEW_DEFAULT_CHARS
        }
    }

    /// Replace the notification queue (wire shared background-task registry).
    pub fn set_notification_queue(&mut self, queue: Arc<Mutex<Vec<String>>>) {
        self.notification_queue = queue;
    }

    pub fn notification_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.notification_queue.clone()
    }

    /// Push a notice that will be injected before the next LLM call.
    pub fn push_notification(&self, text: impl Into<String>) {
        Self::push_queue(&self.notification_queue, text);
    }

    pub fn abort_handle(&self) -> Arc<AtomicBool> {
        self.abort_flag.clone()
    }

    /// Replace the abort flag (e.g. share with a parent job registry for background cancel).
    pub fn set_abort_flag(&mut self, flag: Arc<AtomicBool>) {
        self.abort_flag = flag;
    }

    /// Report completed turns (1-based) for external progress (background jobs).
    pub fn set_turn_progress(&mut self, counter: Option<Arc<AtomicU64>>) {
        self.turn_progress = counter;
    }

    pub fn abort(&self) {
        self.abort_flag.store(true, Ordering::Relaxed);
    }

    pub fn clear_abort(&self) {
        self.abort_flag.store(false, Ordering::Relaxed);
    }

    pub fn is_aborted(&self) -> bool {
        self.abort_flag.load(Ordering::Relaxed)
    }

    pub fn steer(&self, text: impl Into<String>) {
        Self::push_queue(&self.steering_queue, text);
    }

    pub fn follow_up(&self, text: impl Into<String>) {
        Self::push_queue(&self.followup_queue, text);
    }

    pub fn steering_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.steering_queue.clone()
    }

    pub fn followup_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.followup_queue.clone()
    }

    pub fn has_queued_messages(&self) -> bool {
        !self.steering_queue.lock().expect("steering queue lock").is_empty()
            || !self.followup_queue.lock().expect("followup queue lock").is_empty()
    }

    pub fn push_queue(queue: &Arc<Mutex<Vec<String>>>, text: impl Into<String>) {
        queue
            .lock()
            .expect("queue lock")
            .push(text.into());
    }

    pub fn subscribe(&mut self, listener: EventListener) {
        self.listeners.push(listener);
    }

    pub fn clear_listeners(&mut self) {
        self.listeners.clear();
    }

    pub fn tool_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    /// Replace the registered tool set (e.g. Plan mode ↔ Act mode).
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn Tool>>) {
        self.tools = tools;
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub async fn prompt(&mut self, provider: &dyn LlmProvider, text: &str) -> Result<String> {
        self.prompt_user(provider, AgentMessage::user_text(text))
            .await
    }

    /// Prompt with pre-built user message (text and/or images).
    pub async fn prompt_user(
        &mut self,
        provider: &dyn LlmProvider,
        user: AgentMessage,
    ) -> Result<String> {
        debug_assert!(matches!(user, AgentMessage::User(_)));
        self.messages.push(user);
        self.run(provider).await
    }

    /// Prompt with text + local image files `(mime_type, path)`.
    pub async fn prompt_with_images(
        &mut self,
        provider: &dyn LlmProvider,
        text: &str,
        images: Vec<(String, String)>,
    ) -> Result<String> {
        let msg = if images.is_empty() {
            AgentMessage::user_text(text)
        } else {
            AgentMessage::user_with_images(text, images)
        };
        self.prompt_user(provider, msg).await
    }

    pub async fn run(&mut self, provider: &dyn LlmProvider) -> Result<String> {
        self.clear_abort();
        let run_id = new_run_id();
        let wall_start = Instant::now();
        let meta = self.trace_meta.clone();

        self.record_trace(TraceEvent::RunStart {
            ts_ms: now_ms(),
            run_id: run_id.clone(),
            agent: "one".into(),
            agent_version: meta.agent_version.clone(),
            provider: Some(provider.name().to_string()),
            model: Some(provider.model().to_string()),
            task_id: meta.task_id.clone(),
            config: meta.config.clone(),
            session_id: meta.session_id.clone(),
            user_id: meta.user_id.clone(),
            trace_full: meta.trace_full,
        });

        self.emit(AgentEvent::AgentStart);
        if let Some(hooks) = &self.hooks {
            hooks.on_agent_start().await;
        }
        self.is_busy = true;
        let start_len = self.messages.len();
        let mut final_text;
        let mut turns_done = 0usize;

        for turn in 0..self.config.max_turns {
            if self.is_aborted() {
                return self
                    .finish_aborted(start_len, &run_id, wall_start, turns_done)
                    .await;
            }

            self.drain_steering();
            // Claude-style: background task completions appear as conversation notices.
            self.drain_notifications();
            // Progress: report the turn about to run (1-based) for job UIs.
            if let Some(p) = &self.turn_progress {
                p.store((turn as u64) + 1, Ordering::Relaxed);
            }
            self.emit(AgentEvent::TurnStart { turn });
            if let Some(hooks) = &self.hooks {
                hooks.on_turn_start(turn).await;
            }

            let tools_n = self.tools.len();
            let message_count = self.messages.len();
            self.record_trace(TraceEvent::TurnStart {
                ts_ms: now_ms(),
                run_id: run_id.clone(),
                turn,
                message_count,
                tools_n,
                last_prompt_tokens: (self.last_prompt_tokens > 0)
                    .then_some(self.last_prompt_tokens),
            });

            let request = CompletionRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.messages.clone(),
                tools: self.tool_definitions(),
                thinking_level: self.config.thinking_level,
            };

            // Default: last user turn (short). --trace-full: system + recent messages.
            let input_preview = if self.trace_meta.trace_full {
                crate::trace::llm_input_preview(
                    &request.system_prompt,
                    &request.messages,
                    self.preview_limit(),
                )
            } else {
                crate::trace::last_user_preview(&request.messages, self.preview_limit())
            };
            self.record_trace(TraceEvent::LlmRequest {
                ts_ms: now_ms(),
                run_id: run_id.clone(),
                turn,
                message_count: request.messages.len(),
                tools_n: request.tools.len(),
                system_prompt_len: request.system_prompt.len(),
                input_preview,
            });

            let llm_start = Instant::now();
            let ttft_ms: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
            let response = {
                let listeners: Vec<_> = self.listeners.iter().collect();
                let ttft = ttft_ms.clone();
                let llm_start_for_cb = llm_start;
                provider
                    .complete_streaming(
                        request,
                        &mut |event| {
                            // First stream delta → time-to-first-token.
                            if ttft.lock().expect("ttft").is_none() {
                                *ttft.lock().expect("ttft") =
                                    Some(llm_start_for_cb.elapsed().as_millis() as u64);
                            }
                            match event {
                                crate::streaming::StreamEvent::TextDelta(delta) => {
                                    let agent_event = AgentEvent::TextDelta {
                                        delta: delta.clone(),
                                    };
                                    for listener in &listeners {
                                        listener(&agent_event);
                                    }
                                }
                                crate::streaming::StreamEvent::ThinkingDelta(delta) => {
                                    let agent_event = AgentEvent::ThinkingDelta {
                                        delta: delta.clone(),
                                    };
                                    for listener in &listeners {
                                        listener(&agent_event);
                                    }
                                }
                            }
                        },
                        Some(&self.abort_flag),
                    )
                    .await
            };

            let response = match response {
                Ok(r) => r,
                Err(err) => {
                    let err = map_provider_error(err);
                    self.record_trace(TraceEvent::RunEnd {
                        ts_ms: now_ms(),
                        run_id: run_id.clone(),
                        status: TraceRunStatus::Error,
                        turns: turns_done,
                        wall_ms: wall_start.elapsed().as_millis() as u64,
                        usage: self.token_usage,
                        final_text_len: None,
                        final_text_preview: None,
                        error: Some(err.to_string()),
                    });
                    self.is_busy = false;
                    if let Some(hooks) = &self.hooks {
                        hooks.on_agent_end().await;
                    }
                    return Err(err);
                }
            };

            let latency_ms = llm_start.elapsed().as_millis() as u64;
            let ttft = ttft_ms.lock().expect("ttft").clone();
            let tool_calls = extract_tool_calls(&response.content);
            let text = extract_text(&response.content);
            let text_len = text.len();
            let thinking_len = extract_thinking_len(&response.content);
            // Always attach generation text (or tool-call summary) so Langfuse
            // observation.output is readable without --trace-full.
            // --trace-full only raises the preview budget (16k vs 240 chars).
            let output_preview = crate::trace::llm_output_preview(
                &text,
                &tool_calls,
                self.preview_limit(),
            );

            self.record_trace(TraceEvent::LlmResponse {
                ts_ms: now_ms(),
                run_id: run_id.clone(),
                turn,
                latency_ms,
                ttft_ms: ttft,
                stop_reason: stop_reason_label(response.stop_reason).into(),
                tool_calls_n: tool_calls.len(),
                text_len,
                thinking_len,
                usage: response.usage,
                provider: response.provider.clone(),
                model: response.model.clone(),
                output_preview,
            });

            if !response.usage.is_zero() {
                self.token_usage.add_assign(&response.usage);
                let ctx = response.usage.context_size_tokens();
                if ctx > 0 {
                    self.last_prompt_tokens = ctx;
                }
            }

            turns_done = turn + 1;

            if self.is_aborted() || response.stop_reason == StopReason::Aborted {
                let assistant = AgentMessage::Assistant(AssistantMessage {
                    content: response.content.clone(),
                    provider: response.provider.clone(),
                    model: response.model.clone(),
                    stop_reason: StopReason::Aborted,
                    timestamp: crate::message::now_ms(),
                });
                self.messages.push(assistant);
                return self
                    .finish_aborted(start_len, &run_id, wall_start, turns_done)
                    .await;
            }

            let assistant = AgentMessage::Assistant(AssistantMessage {
                content: response.content.clone(),
                provider: response.provider.clone(),
                model: response.model.clone(),
                stop_reason: response.stop_reason,
                timestamp: crate::message::now_ms(),
            });
            self.messages.push(assistant.clone());

            let mut tool_results = Vec::new();

            if tool_calls.is_empty() {
                final_text = extract_text(&response.content);
                self.emit(AgentEvent::TurnEnd {
                    turn,
                    assistant,
                    tool_results,
                });
                if let Some(hooks) = &self.hooks {
                    hooks.on_turn_end(turn).await;
                }
                if self.drain_followup() {
                    continue;
                }
                self.is_busy = false;
                self.emit(AgentEvent::AgentEnd {
                    new_messages: self.messages[start_len..].to_vec(),
                });
                if let Some(hooks) = &self.hooks {
                    hooks.on_agent_end().await;
                }
                let final_text_preview = if self.trace_meta.trace_full {
                    crate::trace::text_preview(&final_text, self.preview_limit())
                } else {
                    crate::trace::text_preview(&final_text, crate::trace::PREVIEW_DEFAULT_CHARS)
                };
                self.record_trace(TraceEvent::RunEnd {
                    ts_ms: now_ms(),
                    run_id: run_id.clone(),
                    status: TraceRunStatus::Ok,
                    turns: turns_done,
                    wall_ms: wall_start.elapsed().as_millis() as u64,
                    usage: self.token_usage,
                    final_text_len: Some(final_text.len()),
                    final_text_preview,
                    error: None,
                });
                return Ok(final_text);
            }

            // Gate sequentially (HITL Ask is single-slot), then run allowed tools
            // concurrently. Steer/abort mid-batch → synthetic error toolResults so
            // tool_call / tool_result pairs stay valid for the provider.
            match self
                .run_tool_batch(&tool_calls, turn, &run_id, &mut tool_results)
                .await
            {
                ToolBatchOutcome::Aborted => {
                    self.emit(AgentEvent::TurnEnd {
                        turn,
                        assistant: assistant.clone(),
                        tool_results,
                    });
                    if let Some(hooks) = &self.hooks {
                        hooks.on_turn_end(turn).await;
                    }
                    return self
                        .finish_aborted(start_len, &run_id, wall_start, turns_done)
                        .await;
                }
                ToolBatchOutcome::Continue => {}
            }

            self.emit(AgentEvent::TurnEnd {
                turn,
                assistant,
                tool_results,
            });
            if let Some(hooks) = &self.hooks {
                hooks.on_turn_end(turn).await;
            }
        }

        self.is_busy = false;
        if let Some(hooks) = &self.hooks {
            hooks.on_agent_end().await;
        }
        self.record_trace(TraceEvent::RunEnd {
            ts_ms: now_ms(),
            run_id,
            status: TraceRunStatus::MaxTurns,
            turns: turns_done,
            wall_ms: wall_start.elapsed().as_millis() as u64,
            usage: self.token_usage,
            final_text_len: None,
            final_text_preview: None,
            error: Some(format!("max turns ({})", self.config.max_turns)),
        });
        self.emit(AgentEvent::AgentEnd {
            new_messages: self.messages[start_len..].to_vec(),
        });
        Err(OneError::MaxTurns {
            max: self.config.max_turns,
        })
    }

    async fn finish_aborted(
        &mut self,
        start_len: usize,
        run_id: &str,
        wall_start: Instant,
        turns: usize,
    ) -> Result<String> {
        self.is_busy = false;
        self.emit(AgentEvent::AgentEnd {
            new_messages: self.messages[start_len..].to_vec(),
        });
        if let Some(hooks) = &self.hooks {
            hooks.on_agent_end().await;
        }
        self.record_trace(TraceEvent::RunEnd {
            ts_ms: now_ms(),
            run_id: run_id.to_string(),
            status: TraceRunStatus::Aborted,
            turns,
            wall_ms: wall_start.elapsed().as_millis() as u64,
            usage: self.token_usage,
            final_text_len: None,
            final_text_preview: None,
            error: Some("aborted".into()),
        });
        Err(OneError::Aborted)
    }

    fn drain_steering(&mut self) {
        let mut queue = self.steering_queue.lock().expect("steering queue lock");
        // Preserve FIFO order (push to end, drain from front).
        let items: Vec<_> = queue.drain(..).collect();
        for text in items {
            self.messages.push(AgentMessage::user_text(text));
        }
    }

    fn drain_notifications(&mut self) {
        let mut queue = self
            .notification_queue
            .lock()
            .expect("notification queue lock");
        let items: Vec<_> = queue.drain(..).collect();
        drop(queue);
        for text in items {
            self.messages.push(AgentMessage::user_text(text));
        }
    }

    fn drain_followup(&mut self) -> bool {
        let mut queue = self.followup_queue.lock().expect("followup queue lock");
        if queue.is_empty() {
            return false;
        }
        let items: Vec<_> = queue.drain(..).collect();
        drop(queue);
        for text in items {
            self.messages.push(AgentMessage::user_text(text));
        }
        true
    }

    /// Gate + execute a batch of tool calls from one assistant turn.
    async fn run_tool_batch(
        &mut self,
        tool_calls: &[ToolCall],
        turn: usize,
        run_id: &str,
        tool_results: &mut Vec<AgentMessage>,
    ) -> ToolBatchOutcome {
        let mut slots: Vec<ToolSlot> = Vec::with_capacity(tool_calls.len());

        for (i, call) in tool_calls.iter().enumerate() {
            if self.is_aborted() {
                self.execute_slots(&mut slots, turn, run_id, tool_results).await;
                for call in &tool_calls[i..] {
                    self.emit_synthetic_skip(
                        call,
                        turn,
                        run_id,
                        "aborted before tool execution",
                        tool_results,
                    );
                }
                return ToolBatchOutcome::Aborted;
            }
            if i > 0 && self.has_steering() {
                // Finish already-gated tools, skip the rest with paired error results.
                self.execute_slots(&mut slots, turn, run_id, tool_results).await;
                for call in &tool_calls[i..] {
                    self.emit_synthetic_skip(
                        call,
                        turn,
                        run_id,
                        "skipped: user steering message queued",
                        tool_results,
                    );
                }
                return ToolBatchOutcome::Continue;
            }

            let (args_bytes, preview) = args_preview(&call.arguments, self.preview_limit());
            self.record_trace(TraceEvent::ToolStart {
                ts_ms: now_ms(),
                run_id: run_id.to_string(),
                turn,
                call_id: call.id.clone(),
                name: call.name.clone(),
                args_bytes,
                args_preview: preview,
            });
            self.emit(AgentEvent::ToolExecutionStart {
                tool_call: call.clone(),
            });

            match self.gate_tool(call, run_id, turn).await {
                GateOutcome::Allow {
                    effective,
                    gate,
                    tool,
                } => {
                    slots.push(ToolSlot::Pending {
                        original: call.clone(),
                        effective,
                        gate,
                        tool,
                    });
                }
                GateOutcome::Deny { message, gate } => {
                    slots.push(ToolSlot::Done {
                        original: call.clone(),
                        output: ToolOutput::text(message),
                        is_error: true,
                        gate,
                        duration_ms: 0,
                    });
                }
            }
        }

        self.execute_slots(&mut slots, turn, run_id, tool_results).await;
        if self.is_aborted() {
            return ToolBatchOutcome::Aborted;
        }
        ToolBatchOutcome::Continue
    }

    /// Execute pending slots, then emit ToolEnd / ToolResult in original order.
    ///
    /// **Parallelism policy:** consecutive read-only tools (`read`/`grep`/`find`/…)
    /// run via `join_all`. Side-effecting tools (`write`/`edit`/`bash`/MCP/…) run
    /// one at a time so they cannot race on the same files or shell state.
    async fn execute_slots(
        &mut self,
        slots: &mut Vec<ToolSlot>,
        turn: usize,
        run_id: &str,
        tool_results: &mut Vec<AgentMessage>,
    ) {
        let n = slots.len();
        let mut i = 0;
        while i < n {
            // Already finished (e.g. gate deny).
            if matches!(&slots[i], ToolSlot::Done { .. }) {
                i += 1;
                continue;
            }

            let side_effect = match &slots[i] {
                ToolSlot::Pending { original, .. } => !is_parallel_safe_tool(&original.name),
                ToolSlot::Done { .. } => false,
            };

            if side_effect {
                self.run_pending_at(slots, i).await;
                i += 1;
                continue;
            }

            // Gather consecutive parallel-safe pending indices until a side-effecting pending.
            let mut batch: Vec<usize> = Vec::new();
            let mut k = i;
            while k < n {
                match &slots[k] {
                    ToolSlot::Pending { original, .. } if is_parallel_safe_tool(&original.name) => {
                        batch.push(k);
                        k += 1;
                    }
                    ToolSlot::Pending { .. } => break, // write/bash/MCP — stop before it
                    ToolSlot::Done { .. } => {
                        k += 1; // skip denials; keep collecting later reads
                    }
                }
            }

            if batch.is_empty() {
                // Should not happen (i was Pending parallel-safe).
                i += 1;
                continue;
            }

            self.run_pending_batch(slots, &batch).await;
            i = k;
        }

        for slot in std::mem::take(slots) {
            match slot {
                ToolSlot::Done {
                    original,
                    output,
                    is_error,
                    gate,
                    duration_ms,
                } => {
                    self.finish_tool_result(
                        &original,
                        turn,
                        run_id,
                        output,
                        is_error,
                        gate,
                        duration_ms,
                        tool_results,
                    );
                }
                ToolSlot::Pending { original, .. } => {
                    self.finish_tool_result(
                        &original,
                        turn,
                        run_id,
                        ToolOutput::text("internal error: tool not executed"),
                        true,
                        None,
                        0,
                        tool_results,
                    );
                }
            }
        }
    }

    async fn run_pending_at(&mut self, slots: &mut [ToolSlot], index: usize) {
        let (original, effective, gate, tool) = match &slots[index] {
            ToolSlot::Pending {
                original,
                effective,
                gate,
                tool,
            } => (
                original.clone(),
                effective.clone(),
                gate.clone(),
                Arc::clone(tool),
            ),
            ToolSlot::Done { .. } => return,
        };
        if self.is_aborted() {
            slots[index] = ToolSlot::Done {
                original,
                output: ToolOutput::text("aborted before tool execution"),
                is_error: true,
                gate,
                duration_ms: 0,
            };
            return;
        }
        let start = Instant::now();
        // Race tool work against Esc so long bash/network tools stop ~50ms after abort
        // (bash uses kill_on_drop; dropping the future cancels the child).
        let res = match crate::streaming::race_abort(
            tool.execute(&effective),
            Some(&self.abort_flag),
        )
        .await
        {
            Ok(res) => res,
            Err(()) => Err(OneError::Aborted),
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        let (output, is_error) = match res {
            Ok(output) => {
                let failed = tool_output_indicates_error(&original.name, &output);
                if let Some(g) = &self.tool_gate {
                    g.after_tool(&effective, &output, failed).await;
                }
                (output, failed)
            }
            Err(OneError::Aborted) => {
                let output = ToolOutput::text("aborted");
                if let Some(g) = &self.tool_gate {
                    g.after_tool(&effective, &output, true).await;
                }
                (output, true)
            }
            Err(err) => {
                let output = ToolOutput::text(err.to_string());
                if let Some(g) = &self.tool_gate {
                    g.after_tool(&effective, &output, true).await;
                }
                (output, true)
            }
        };
        slots[index] = ToolSlot::Done {
            original,
            output,
            is_error,
            gate,
            duration_ms,
        };
    }

    async fn run_pending_batch(&mut self, slots: &mut [ToolSlot], indices: &[usize]) {
        if indices.is_empty() {
            return;
        }
        if indices.len() == 1 {
            self.run_pending_at(slots, indices[0]).await;
            return;
        }

        let mut jobs: Vec<(usize, ToolCall, ToolCall, Option<TraceGateDecision>, Arc<dyn Tool>)> =
            Vec::with_capacity(indices.len());
        for &i in indices {
            if let ToolSlot::Pending {
                original,
                effective,
                gate,
                tool,
            } = &slots[i]
            {
                jobs.push((
                    i,
                    original.clone(),
                    effective.clone(),
                    gate.clone(),
                    Arc::clone(tool),
                ));
            }
        }

        let start = Instant::now();
        let abort = self.abort_flag.clone();
        let futs: Vec<_> = jobs
            .iter()
            .map(|(_, _, effective, _, tool)| {
                let tool = Arc::clone(tool);
                let effective = effective.clone();
                let abort = abort.clone();
                async move {
                    match crate::streaming::race_abort(
                        tool.execute(&effective),
                        Some(abort.as_ref()),
                    )
                    .await
                    {
                        Ok(res) => res,
                        Err(()) => Err(OneError::Aborted),
                    }
                }
            })
            .collect();
        let results = futures::future::join_all(futs).await;
        let elapsed = start.elapsed().as_millis() as u64;

        for ((i, original, effective, gate, _tool), res) in jobs.into_iter().zip(results) {
            let (output, is_error) = match res {
                Ok(output) => {
                    let failed = tool_output_indicates_error(&original.name, &output);
                    if let Some(g) = &self.tool_gate {
                        g.after_tool(&effective, &output, failed).await;
                    }
                    (output, failed)
                }
                Err(OneError::Aborted) => {
                    let output = ToolOutput::text("aborted");
                    if let Some(g) = &self.tool_gate {
                        g.after_tool(&effective, &output, true).await;
                    }
                    (output, true)
                }
                Err(err) => {
                    let output = ToolOutput::text(err.to_string());
                    if let Some(g) = &self.tool_gate {
                        g.after_tool(&effective, &output, true).await;
                    }
                    (output, true)
                }
            };
            slots[i] = ToolSlot::Done {
                original,
                output,
                is_error,
                gate,
                duration_ms: elapsed,
            };
        }
    }

    fn has_steering(&self) -> bool {
        !self
            .steering_queue
            .lock()
            .expect("steering queue lock")
            .is_empty()
    }

    async fn gate_tool(&self, call: &ToolCall, run_id: &str, turn: usize) -> GateOutcome {
        let mut effective = call.clone();
        let mut gate_decision = None;
        if let Some(gate) = &self.tool_gate {
            match gate.check(&effective).await {
                ToolGateDecision::Allow => {
                    gate_decision = Some(TraceGateDecision::Allow);
                    self.record_trace(TraceEvent::Gate {
                        ts_ms: now_ms(),
                        run_id: run_id.to_string(),
                        turn,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        decision: TraceGateDecision::Allow,
                        message: None,
                    });
                }
                ToolGateDecision::Rewrite { arguments } => {
                    gate_decision = Some(TraceGateDecision::Rewrite);
                    self.record_trace(TraceEvent::Gate {
                        ts_ms: now_ms(),
                        run_id: run_id.to_string(),
                        turn,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        decision: TraceGateDecision::Rewrite,
                        message: None,
                    });
                    effective.arguments = arguments;
                }
                ToolGateDecision::Deny { message } => {
                    self.record_trace(TraceEvent::Gate {
                        ts_ms: now_ms(),
                        run_id: run_id.to_string(),
                        turn,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        decision: TraceGateDecision::Deny,
                        message: Some(message.clone()),
                    });
                    return GateOutcome::Deny {
                        message,
                        gate: Some(TraceGateDecision::Deny),
                    };
                }
            }
        }

        match self
            .tools
            .iter()
            .find(|tool| tool.definition().name == effective.name)
        {
            Some(tool) => GateOutcome::Allow {
                effective,
                gate: gate_decision,
                tool: Arc::clone(tool),
            },
            None => GateOutcome::Deny {
                message: format!("tool not registered: {}", effective.name),
                gate: None,
            },
        }
    }

    /// Emit ToolStart + error ToolResult for a call that never ran (steer/abort).
    fn emit_synthetic_skip(
        &mut self,
        call: &ToolCall,
        turn: usize,
        run_id: &str,
        reason: &str,
        tool_results: &mut Vec<AgentMessage>,
    ) {
        let (args_bytes, preview) = args_preview(&call.arguments, self.preview_limit());
        self.record_trace(TraceEvent::ToolStart {
            ts_ms: now_ms(),
            run_id: run_id.to_string(),
            turn,
            call_id: call.id.clone(),
            name: call.name.clone(),
            args_bytes,
            args_preview: preview,
        });
        self.emit(AgentEvent::ToolExecutionStart {
            tool_call: call.clone(),
        });
        self.finish_tool_result(
            call,
            turn,
            run_id,
            ToolOutput::text(reason),
            true,
            None,
            0,
            tool_results,
        );
    }

    fn finish_tool_result(
        &mut self,
        call: &ToolCall,
        turn: usize,
        run_id: &str,
        output: ToolOutput,
        is_error: bool,
        gate_decision: Option<TraceGateDecision>,
        duration_ms: u64,
        tool_results: &mut Vec<AgentMessage>,
    ) {
        let output_text = output.as_text();
        let output_bytes = output_text.len();
        // Same as generation: short preview by default; --trace-full expands budget.
        let output_preview = crate::trace::text_preview(&output_text, self.preview_limit());
        self.record_trace(TraceEvent::ToolEnd {
            ts_ms: now_ms(),
            run_id: run_id.to_string(),
            turn,
            call_id: call.id.clone(),
            name: call.name.clone(),
            duration_ms,
            is_error,
            output_bytes,
            gate: gate_decision,
            output_preview,
        });
        self.emit(AgentEvent::ToolExecutionEnd {
            tool_call: call.clone(),
            output: output.clone(),
            is_error,
        });
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: output.content.clone(),
            is_error,
            timestamp: crate::message::now_ms(),
        });
        self.messages.push(result.clone());
        tool_results.push(result);
    }

    fn emit(&mut self, event: AgentEvent) {
        for listener in &self.listeners {
            listener(&event);
        }
    }
}

/// Slot for concurrent tool execution (gate already applied).
enum ToolSlot {
    Pending {
        original: ToolCall,
        effective: ToolCall,
        gate: Option<TraceGateDecision>,
        tool: Arc<dyn Tool>,
    },
    Done {
        original: ToolCall,
        output: ToolOutput,
        is_error: bool,
        gate: Option<TraceGateDecision>,
        duration_ms: u64,
    },
}

enum ToolBatchOutcome {
    Continue,
    Aborted,
}

enum GateOutcome {
    Allow {
        effective: ToolCall,
        gate: Option<TraceGateDecision>,
        tool: Arc<dyn Tool>,
    },
    Deny {
        message: String,
        gate: Option<TraceGateDecision>,
    },
}


/// Tools that only observe state and are safe to run concurrently with each other.
///
/// Everything else (writes, shell, MCP, ask_user, plan tools, unknown names) runs serially.
pub fn is_parallel_safe_tool(name: &str) -> bool {
    matches!(
        name,
        // `task` is explore-only (read-only research) in MVP → concurrent-safe.
        // When general/write subagents land, keep them serial via a different
        // name or gate classification on mode.
        "read" | "grep" | "find" | "ls" | "bash_output" | "web_search" | "web_fetch" | "task"
    )
}

/// Detect soft failures that still return `Ok(ToolOutput)` (e.g. bash exit ≠ 0, MCP is_error).
fn tool_output_indicates_error(tool_name: &str, output: &ToolOutput) -> bool {
    // Generic details flags (MCP, tools that report ok/is_error).
    if let Some(details) = &output.details {
        if details.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            return true;
        }
        if details.get("ok").and_then(|v| v.as_bool()) == Some(false) {
            // Background bash start / still-running snapshots are handled below.
            let background = details
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let running = details
                .get("running")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !background && !running {
                return true;
            }
        }
    }

    match tool_name {
        "bash" | "shell" | "bash_output" => {
            if let Some(details) = &output.details {
                if details
                    .get("background")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return false;
                }
                if details
                    .get("running")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return false;
                }
                if let Some(ok) = details.get("ok").and_then(|v| v.as_bool()) {
                    return !ok;
                }
                match details.get("exitCode") {
                    Some(v) if v.is_null() => return true,
                    Some(v) => {
                        if let Some(code) = v.as_i64() {
                            return code != 0;
                        }
                    }
                    None => {}
                }
            }
            let text = output.as_text();
            if let Some(rest) = text.strip_prefix("exit ") {
                let code = rest
                    .split(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                if code == "signal" {
                    return true;
                }
                if let Ok(n) = code.parse::<i64>() {
                    return n != 0;
                }
            }
            false
        }
        _ => false,
    }
}

/// Lift string-matched context overflows into the dedicated error variant.
fn map_provider_error(err: OneError) -> OneError {
    match err {
        OneError::Provider(msg) if crate::compaction::is_context_overflow_error(&msg) => {
            OneError::ContextOverflow(msg)
        }
        other => other,
    }
}

pub fn extract_tool_calls(content: &[ContentBlock]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, name, arguments } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

pub fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn extract_thinking_len(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking, .. } => Some(thinking.len()),
            _ => None,
        })
        .sum()
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "tool_use",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

/// Helper for providers that stream text deltas to listeners.
pub async fn drain_text_deltas<S>(mut stream: S, on_delta: &mut dyn FnMut(&str))
where
    S: futures::Stream<Item = String> + Unpin,
{
    while let Some(delta) = stream.next().await {
        on_delta(&delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_safe_tools_are_read_only() {
        assert!(is_parallel_safe_tool("read"));
        assert!(is_parallel_safe_tool("grep"));
        assert!(is_parallel_safe_tool("find"));
        assert!(is_parallel_safe_tool("ls"));
        assert!(is_parallel_safe_tool("web_search"));
        assert!(is_parallel_safe_tool("task")); // explore MVP concurrent
        assert!(!is_parallel_safe_tool("write"));
        assert!(!is_parallel_safe_tool("edit"));
        assert!(!is_parallel_safe_tool("bash"));
        assert!(!is_parallel_safe_tool("ask_user"));
        assert!(!is_parallel_safe_tool("mcp_something"));
        assert!(!is_parallel_safe_tool("exit_plan_mode"));
    }

    #[test]
    fn token_usage_total_does_not_double_count_cache() {
        let u = TokenUsage {
            input_tokens: 1000,
            output_tokens: 50,
            cache_read_tokens: 800, // OpenAI: subset of input
            cache_write_tokens: 0,
        };
        assert_eq!(u.total(), 1050);
        assert_eq!(u.uncached_input_tokens(), 200);
        assert_eq!(u.prompt_tokens_expanded(), 1800); // Anthropic-style only
        // OpenAI-style: context size is input (cache already inside).
        assert_eq!(u.context_size_tokens(), 1000);
    }

    #[test]
    fn context_size_tokens_anthropic_style() {
        let u = TokenUsage {
            input_tokens: 200,
            output_tokens: 10,
            cache_read_tokens: 800,
            cache_write_tokens: 50,
        };
        assert_eq!(u.context_size_tokens(), 1050); // input + read + write
    }

    #[test]
    fn token_usage_is_zero_sees_cache_only() {
        let u = TokenUsage {
            cache_read_tokens: 10,
            ..Default::default()
        };
        assert!(!u.is_zero());
        assert_eq!(u.total(), 0);
    }

    #[tokio::test]
    async fn abort_stops_agent_run() {
        struct AbortingProvider;

        #[async_trait::async_trait]
        impl LlmProvider for AbortingProvider {
            fn name(&self) -> &str {
                "abort-test"
            }

            fn model(&self) -> &str {
                "test"
            }

            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                unreachable!("streaming only")
            }

            async fn complete_streaming(
                &self,
                _request: CompletionRequest,
                on_event: &mut (dyn FnMut(crate::streaming::StreamEvent) + Send),
                _abort: Option<&AtomicBool>,
            ) -> Result<CompletionResponse> {
                on_event(crate::streaming::StreamEvent::TextDelta("partial".to_string()));
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: "partial".to_string(),
                    }],
                    stop_reason: StopReason::Aborted,
                    usage: TokenUsage::default(),
                })
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let result = agent.prompt(&AbortingProvider, "hi").await;
        assert!(matches!(result, Err(OneError::Aborted)));
        assert!(!agent.is_busy);
        assert_eq!(agent.messages.len(), 2);
    }

    #[tokio::test]
    async fn abort_cancels_in_flight_tool_quickly() {
        use crate::tool::{Tool, ToolDefinition};
        use std::time::Duration;

        struct SlowTool;

        #[async_trait::async_trait]
        impl Tool for SlowTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "slow".into(),
                    description: "sleeps".into(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }

            async fn execute(&self, _call: &ToolCall) -> Result<ToolOutput> {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(ToolOutput::text("done"))
            }
        }

        struct ToolThenStopProvider {
            calls: AtomicU64,
        }

        #[async_trait::async_trait]
        impl LlmProvider for ToolThenStopProvider {
            fn name(&self) -> &str {
                "abort-tool-test"
            }

            fn model(&self) -> &str {
                "test"
            }

            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                unreachable!()
            }

            async fn complete_streaming(
                &self,
                _request: CompletionRequest,
                _on_event: &mut (dyn FnMut(crate::streaming::StreamEvent) + Send),
                _abort: Option<&AtomicBool>,
            ) -> Result<CompletionResponse> {
                let n = self.calls.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().to_string(),
                        model: self.model().to_string(),
                        content: vec![ContentBlock::ToolCall {
                            id: "c1".into(),
                            name: "slow".into(),
                            arguments: serde_json::json!({}),
                        }],
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().to_string(),
                        model: self.model().to_string(),
                        content: vec![ContentBlock::Text {
                            text: "should not reach".into(),
                        }],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                    })
                }
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), vec![Arc::new(SlowTool)]);
        let handle = agent.abort_handle();
        let provider = ToolThenStopProvider {
            calls: AtomicU64::new(0),
        };

        let run = tokio::spawn(async move { agent.prompt(&provider, "go").await });
        // Let the tool start sleeping, then abort.
        tokio::time::sleep(Duration::from_millis(80)).await;
        handle.store(true, Ordering::Relaxed);

        let result = tokio::time::timeout(Duration::from_millis(500), run)
            .await
            .expect("tool abort should finish within poll interval")
            .expect("join");
        assert!(matches!(result, Err(OneError::Aborted)));
    }

    #[test]
    fn extracts_tool_calls_from_content() {
        let content = vec![
            ContentBlock::Text {
                text: "checking".to_string(),
            },
            ContentBlock::ToolCall {
                id: "1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "ls" }),
            },
        ];

        let calls = extract_tool_calls(&content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
    }

    #[test]
    fn background_start_is_not_error() {
        let output = ToolOutput::text_with_details(
            "Background task started\ntask_id: bg_1",
            serde_json::json!({ "background": true, "ok": true, "task_id": "bg_1" }),
        );
        assert!(!tool_output_indicates_error("bash", &output));
    }

    #[test]
    fn bash_output_running_is_not_error() {
        let output = ToolOutput::text_with_details(
            "status: running",
            serde_json::json!({ "running": true, "ok": true, "status": "running" }),
        );
        assert!(!tool_output_indicates_error("bash_output", &output));
    }

    #[tokio::test]
    async fn injects_notifications_before_llm_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct NoticeProvider {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for NoticeProvider {
            fn name(&self) -> &str {
                "notice"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let has_notice = request.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => u
                            .content
                            .as_plain_text()
                            .contains("[Background task completed]"),
                        _ => false,
                    });
                    assert!(has_notice, "notification should be injected before LLM call");
                }
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                })
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.push_notification(
            "[Background task completed]\ntask_id: bg_test_1\nexit: 0\n",
        );
        let out = agent
            .prompt(
                &NoticeProvider {
                    calls: AtomicUsize::new(0),
                },
                "hi",
            )
            .await
            .expect("run");
        assert_eq!(out, "done");
        assert!(agent.messages.len() >= 3);
    }
}