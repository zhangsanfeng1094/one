use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

/// Mode for waiting on multiple targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitInterestMode {
    #[default]
    All,
    Any,
}

impl WaitInterestMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }
}

pub struct RegisteredWaiter {
    notified: Pin<Box<tokio::sync::futures::OwnedNotified>>,
}

impl RegisteredWaiter {
    pub fn new(notify: Arc<tokio::sync::Notify>) -> Self {
        let mut notified = Box::pin(notify.notified_owned());
        notified.as_mut().enable();
        Self { notified }
    }
}

impl Future for RegisteredWaiter {
    type Output = ();
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.notified.as_mut().poll(cx)
    }
}
///
/// Follows subscribe-before-recheck pattern to prevent lost wakeups.
pub trait TaskWaitWaiter: Send + Sync {
    /// Check whether target is in terminal state right now.
    fn is_terminal(&self, id: &str) -> bool;

    /// Subscribe to terminal notification for `id` before rechecking state.
    /// Awaiting this future resolves when the task signals or completes.
    fn subscribe_terminal<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Active wait interest registered by `wait_tasks`.
#[derive(Clone)]
pub struct WaitInterest {
    pub ids: Vec<String>,
    pub mode: WaitInterestMode,
    pub waiter: Arc<dyn TaskWaitWaiter>,
}

impl WaitInterest {
    pub fn new(ids: Vec<String>, mode: WaitInterestMode, waiter: Arc<dyn TaskWaitWaiter>) -> Self {
        Self { ids, mode, waiter }
    }

    /// Check if the wait interest is currently satisfied.
    pub fn is_satisfied(&self) -> bool {
        if self.ids.is_empty() {
            return true;
        }
        match self.mode {
            WaitInterestMode::All => self.ids.iter().all(|id| self.waiter.is_terminal(id)),
            WaitInterestMode::Any => self.ids.iter().any(|id| self.waiter.is_terminal(id)),
        }
    }
}

use crate::compaction::{
    can_fork_summarize_prefix, compacted_live_messages, compaction_request_messages,
    extractive_summary, messages_fingerprint, should_compact_tokens, split_for_compaction_forced,
    summarization_prompt, tokens_for_compaction, CompactApplied, CompactTrigger, PrunedToolResult,
};
use crate::error::{OneError, Result};
use crate::events::{AgentEvent, EventListener};
use crate::hooks::{AgentHooks, StopDecision};
use crate::message::{
    now_ms, AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage,
    UserContent, UserMessage,
};
use crate::tool::{resolve_tool_name, Tool, ToolCall, ToolOutput};
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
pub use one_prompt::builtin::DEFAULT_SYSTEM_PROMPT;

mod react;

/// Reasoning / extended-thinking intensity (provider-specific mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// Extra LLM samples after a retryable completion failure.
///
/// This covers blank model completions and temporary provider errors such as
/// capacity, rate limiting, or unavailable upstreams. Total attempts = 1 +
/// this value. Each retry is delayed with a capped backoff so we do not hammer
/// a provider that is already overloaded.
pub const DEFAULT_EMPTY_RESPONSE_RETRIES: usize = 10;

const RETRY_BACKOFF_SECS: &[u64] = &[2, 3, 5, 8, 13, 20];

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Maximum LLM/tool iterations for one user request. `0` means unlimited:
    /// interactive sessions continue until the model completes, the user aborts,
    /// or another execution guard ends the run.
    pub max_turns: usize,
    pub thinking_level: ThinkingLevel,
    /// Request-side only: attach `provider.server_tools()` (hosted web/x search)
    /// on the main completion. When false, do not declare them — local function
    /// `web_search` may still be registered by the host.
    ///
    /// Does **not** gate response handling: `web_search_call` events and
    /// `citations` are always parsed if the upstream/proxy returns them.
    pub server_search: bool,
    /// How many times to retry a blank model turn or temporary provider error.
    /// Reasoning-only turns count as empty (same as Grok Build).
    pub empty_response_retries: usize,
    /// Optional compaction/pruning configuration applied during intra-turn tool loops.
    pub compaction_config: Option<crate::compaction::CompactionConfig>,
    /// Legacy batch concurrency cap. ReAct applies it to reads; Event also
    /// applies it to mutations of different files.
    pub max_parallel_readonly_tools: usize,
    /// Optional per-provider/model reminders for serial read-only exploration.
    pub batch_exploration: Vec<BatchExplorationRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BatchExplorationRule {
    pub provider: String,
    pub model: String,
    /// Omitted means any thinking level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    /// Consecutive single read-only tool batches before the one-time reminder.
    pub after_single_reads: usize,
}

/// Default cap for consecutive parallel-safe tools (`read` / `grep` / …).
pub const DEFAULT_MAX_PARALLEL_READONLY_TOOLS: usize = 8;

const BATCH_EXPLORATION_REMINDER: &str =
    "若接下来有相互独立的检查，请在同一响应中发出；有依赖的检查按顺序执行。";

fn batch_exploration_threshold(
    rules: &[BatchExplorationRule],
    provider: &dyn LlmProvider,
    level: ThinkingLevel,
) -> Option<usize> {
    rules
        .iter()
        .find(|rule| {
            rule.provider == provider.name()
                && batch_model_matches(&rule.model, provider.model())
                && rule
                    .thinking_level
                    .is_none_or(|configured| configured == level)
                && rule.after_single_reads > 0
        })
        .map(|rule| rule.after_single_reads)
}

/// Exact model id, plus a single parenthetical suffix (`id` matches `id(medium)`).
fn batch_model_matches(rule_model: &str, actual: &str) -> bool {
    if rule_model == actual {
        return true;
    }
    let Some(rest) = actual.strip_prefix(rule_model) else {
        return false;
    };
    let Some(inner) = rest.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
        return false;
    };
    !inner.is_empty() && !inner.contains('(') && !inner.contains(')')
}

#[derive(Default)]
struct BatchExplorationState {
    consecutive_single_reads: usize,
    reminded: bool,
}

impl BatchExplorationState {
    fn observe(&mut self, calls: &[ToolCall]) {
        if calls.len() == 1 && is_parallel_safe_tool(&calls[0].name) {
            self.consecutive_single_reads += 1;
        } else {
            self.consecutive_single_reads = 0;
        }
    }

    fn take_reminder(&mut self, threshold: usize) -> bool {
        if !self.reminded && self.consecutive_single_reads >= threshold {
            self.reminded = true;
            true
        } else {
            false
        }
    }
}

/// `ONE_MAX_PARALLEL_READONLY_TOOLS` when set to a positive integer.
pub fn max_parallel_readonly_tools_from_env() -> usize {
    std::env::var("ONE_MAX_PARALLEL_READONLY_TOOLS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(DEFAULT_MAX_PARALLEL_READONLY_TOOLS)
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_turns: 0,
            thinking_level: ThinkingLevel::Off,
            server_search: false,
            empty_response_retries: DEFAULT_EMPTY_RESPONSE_RETRIES,
            compaction_config: None,
            max_parallel_readonly_tools: DEFAULT_MAX_PARALLEL_READONLY_TOOLS,
            batch_exploration: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<crate::tool::ToolDefinition>,
    /// Hosted tools to declare on this request only (not client-executed).
    /// Empty when inject is off; response may still contain server tool events.
    pub server_tools: Vec<ServerTool>,
    pub thinking_level: ThinkingLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerTool {
    WebSearch,
    XSearch,
}

impl ServerTool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::XSearch => "x_search",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    pub url: String,
    pub title: String,
    pub start_index: usize,
    pub end_index: usize,
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

    /// Per-field saturating subtraction (e.g. run usage = session total − baseline).
    pub fn saturating_sub(&self, other: &TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.saturating_sub(other.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(other.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_sub(other.cache_read_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_sub(other.cache_write_tokens),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
    }

    /// Best-effort size of the **prompt/context** for this completion
    /// (compaction threshold + UI context %).
    ///
    /// Accounting (provider-dependent):
    /// - **Anthropic-style** (cache fields *disjoint* from `input_tokens`): use
    ///   input + cache_read + cache_write. Detected when `cache_write > 0` or
    ///   `cache_read > input` (cache hit larger than uncached tail — impossible
    ///   under OpenAI subset semantics).
    /// - **OpenAI-style** (`cache_read` ⊆ `input_tokens`): use `input_tokens` alone
    ///   so we never double-count cache.
    pub fn context_size_tokens(&self) -> u64 {
        if self.is_zero() {
            return 0;
        }
        // Disjoint cache (Anthropic / Bedrock-style reporting).
        if self.cache_write_tokens > 0 || self.cache_read_tokens > self.input_tokens {
            return self.prompt_tokens_expanded();
        }
        // Inclusive cache (OpenAI / many OpenAI-compatible): input already full prompt.
        self.input_tokens
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
    /// URL annotations attached to generated output text.
    pub citations: Vec<Citation>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    fn server_tools(&self) -> Vec<ServerTool> {
        Vec::new()
    }

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

/// Summary metadata for automatic compaction performed between tool-loop samples.
///
/// The agent retains the full run transcript separately; hosts use this record to
/// add one durable compaction marker after persisting that transcript.
#[derive(Debug, Clone)]
pub struct IntraTurnCompaction {
    pub summary: String,
    pub applied: CompactApplied,
}

/// Exact model-facing context captured after a prune or compaction mutation.
/// Hosts persist these snapshots separately from the raw user-visible transcript.
#[derive(Debug, Clone)]
pub struct ContextProjectionSnapshot {
    pub id: String,
    pub hash: String,
    pub reason: String,
    pub messages: Vec<AgentMessage>,
    pub pruned_tool_results: Vec<PrunedToolResult>,
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
    /// Original messages appended during the most recently completed run.
    /// This stays separate from the compacted model buffer so session storage can
    /// retain a complete transcript after intra-turn compaction.
    last_run_transcript: Vec<AgentMessage>,
    /// Most recent automatic compaction performed inside the current/last run.
    intra_turn_compaction: Option<IntraTurnCompaction>,
    /// Snapshots of model context mutations made during the current/last run.
    context_projections: Vec<ContextProjectionSnapshot>,
    tools: Vec<Arc<dyn Tool>>,
    listeners: Vec<EventListener>,
    steering_queue: Arc<Mutex<Vec<String>>>,
    followup_queue: Arc<Mutex<Vec<String>>>,
    input_waker: Arc<tokio::sync::Notify>,
    /// Side-channel notices (e.g. background bash completions), drained before each LLM turn.
    /// Injected as user messages with a clear prefix — not tool_results (providers require pairing).
    notification_queue: Arc<Mutex<Vec<String>>>,
    abort_flag: Arc<AtomicBool>,
    /// Optional external turn counter (1-based completed turns) for job progress UIs.
    turn_progress: Option<Arc<AtomicU64>>,
    /// Optional pre-tool permission gate (allow/deny/ask/rewrite).
    tool_gate: Option<Arc<dyn ToolGate>>,
    /// Active wait interest registered by `wait_tasks`.
    wait_interest: Arc<Mutex<Option<WaitInterest>>>,
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
            last_run_transcript: Vec::new(),
            intra_turn_compaction: None,
            context_projections: Vec::new(),
            tools,
            listeners: Vec::new(),
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            followup_queue: Arc::new(Mutex::new(Vec::new())),
            input_waker: Arc::new(tokio::sync::Notify::new()),
            notification_queue: Arc::new(Mutex::new(Vec::new())),
            abort_flag: Arc::new(AtomicBool::new(false)),
            turn_progress: None,
            tool_gate: None,
            wait_interest: Arc::new(Mutex::new(None)),
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

    pub fn wait_interest_handle(&self) -> Arc<Mutex<Option<WaitInterest>>> {
        self.wait_interest.clone()
    }

    pub fn set_wait_interest_handle(&mut self, handle: Arc<Mutex<Option<WaitInterest>>>) {
        self.wait_interest = handle;
    }

    pub fn set_wait_interest(&self, interest: Option<WaitInterest>) {
        *self.wait_interest.lock().expect("wait_interest lock") = interest;
    }

    pub fn clear_wait_interest(&self) {
        *self.wait_interest.lock().expect("wait_interest lock") = None;
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

    /// Update compaction / pruning configuration for intra-run execution.
    pub fn set_compaction_config(&mut self, cfg: Option<crate::compaction::CompactionConfig>) {
        self.config.compaction_config = cfg;
    }

    /// Update session id for the next run (e.g. after `/new` or `/resume`).
    pub fn set_trace_session_id(&mut self, session_id: Option<String>) {
        self.trace_meta.session_id = session_id;
    }

    /// Full, uncompacted message delta from the most recently completed run.
    pub fn last_run_transcript(&self) -> &[AgentMessage] {
        &self.last_run_transcript
    }

    /// Metadata for the latest automatic compaction inside the current/last run.
    pub fn intra_turn_compaction(&self) -> Option<&IntraTurnCompaction> {
        self.intra_turn_compaction.as_ref()
    }

    /// Model-facing snapshots created when the live buffer diverged from raw history.
    pub fn context_projections(&self) -> &[ContextProjectionSnapshot] {
        &self.context_projections
    }

    /// Replace the live context after an external runtime prune and record its provenance.
    pub fn apply_pruned_messages(
        &mut self,
        messages: Vec<AgentMessage>,
        pruned_tool_results: Vec<PrunedToolResult>,
    ) {
        self.messages = messages;
        self.last_prompt_tokens = 0;
        self.record_context_projection("prune", pruned_tool_results);
    }

    fn record_context_projection(
        &mut self,
        reason: impl Into<String>,
        pruned_tool_results: Vec<PrunedToolResult>,
    ) {
        let hash = messages_fingerprint(&self.messages);
        let id = format!("ctx_{}", hash.trim_start_matches("fp1:").replace(':', "_"));
        self.context_projections.push(ContextProjectionSnapshot {
            id,
            hash,
            reason: reason.into(),
            messages: self.messages.clone(),
            pruned_tool_results,
        });
    }

    fn push_message(&mut self, message: AgentMessage) {
        if self.is_busy {
            self.last_run_transcript.push(message.clone());
        }
        self.messages.push(message);
    }

    fn new_messages_since(&self, start_len: usize) -> Vec<AgentMessage> {
        if !self.last_run_transcript.is_empty() {
            self.last_run_transcript.clone()
        } else {
            let start = start_len.min(self.messages.len());
            self.messages[start..].to_vec()
        }
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

    /// Budget for generation observation I/O (full messages + structured output).
    /// Always large enough for multi-turn context; `--trace-full` raises further.
    fn llm_preview_limit(&self) -> usize {
        if self.trace_meta.trace_full {
            // 4× full budget when the operator opted into verbose traces.
            crate::trace::PREVIEW_FULL_CHARS.saturating_mul(4)
        } else {
            crate::trace::PREVIEW_LLM_CHARS
        }
    }

    /// Replace the notification queue (wire shared background-task registry).
    pub fn set_notification_queue(&mut self, queue: Arc<Mutex<Vec<String>>>) {
        self.notification_queue = queue;
    }

    pub fn notification_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.notification_queue.clone()
    }

    /// Queue a notice injected as a tagged user message before the next sample.
    /// `prompt_user` drains this *before* the human `<user_query>` turn.
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
        self.input_waker.notify_one();
    }

    pub fn clear_abort(&self) {
        self.abort_flag.store(false, Ordering::Relaxed);
    }

    pub fn is_aborted(&self) -> bool {
        self.abort_flag.load(Ordering::Relaxed)
    }

    pub fn input_waker_handle(&self) -> Arc<tokio::sync::Notify> {
        self.input_waker.clone()
    }

    pub fn steer(&self, text: impl Into<String>) {
        Self::push_queue(&self.steering_queue, text);
        self.input_waker.notify_one();
    }

    pub fn follow_up(&self, text: impl Into<String>) {
        Self::push_queue(&self.followup_queue, text);
        self.input_waker.notify_one();
    }

    pub fn steering_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.steering_queue.clone()
    }

    pub fn followup_queue_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.followup_queue.clone()
    }

    pub fn has_queued_messages(&self) -> bool {
        !self
            .steering_queue
            .lock()
            .expect("steering queue lock")
            .is_empty()
            || !self
                .followup_queue
                .lock()
                .expect("followup queue lock")
                .is_empty()
    }

    pub fn push_queue(queue: &Arc<Mutex<Vec<String>>>, text: impl Into<String>) {
        queue.lock().expect("queue lock").push(text.into());
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
    ///
    /// Queued `<system-reminder>` notices are drained first so they sit *before*
    /// the human turn (Grok Build `handle_prompt` order). The human text is
    /// wrapped in `<user_query>`.
    pub async fn prompt_user(
        &mut self,
        provider: &dyn LlmProvider,
        mut user: AgentMessage,
    ) -> Result<String> {
        debug_assert!(matches!(user, AgentMessage::User(_)));
        self.drain_notifications();
        if let AgentMessage::User(ref mut u) = user {
            crate::reminder::wrap_user_content_query(&mut u.content);
        }
        self.push_message(user);
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
        // Session-lifetime cumulative; RunEnd reports delta so each Langfuse
        // root observation is per-prompt, not inflated by prior turns.
        let usage_at_run_start = self.token_usage;

        // Root observation input = last user message (trace list / agent graph preview).
        let run_input_preview =
            crate::trace::last_user_preview(&self.messages, self.preview_limit());
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
            input_preview: run_input_preview,
        });

        self.emit(AgentEvent::AgentStart);
        if let Some(hooks) = &self.hooks {
            hooks.on_agent_start().await;
        }
        self.last_run_transcript = self
            .messages
            .iter()
            .rposition(|message| matches!(message, AgentMessage::User(_)))
            .map(|start| self.messages[start..].to_vec())
            .unwrap_or_default();
        self.intra_turn_compaction = None;
        self.context_projections.clear();
        self.is_busy = true;
        let start_len = self.messages.len();
        let mut final_text;
        let mut turns_done = 0usize;
        let mut stop_continuations = 0usize;
        let batch_exploration_threshold = batch_exploration_threshold(
            &self.config.batch_exploration,
            provider,
            self.config.thinking_level,
        );
        let mut batch_exploration = BatchExplorationState::default();
        const MAX_STOP_CONTINUATIONS: usize = 8;

        for turn in 0usize.. {
            if self.config.max_turns > 0 && turn >= self.config.max_turns {
                if self.is_aborted() {
                    return self
                        .finish_aborted(
                            start_len,
                            &run_id,
                            wall_start,
                            turns_done,
                            usage_at_run_start,
                        )
                        .await;
                }
                break;
            }
            if self.is_aborted() {
                return self
                    .finish_aborted(
                        start_len,
                        &run_id,
                        wall_start,
                        turns_done,
                        usage_at_run_start,
                    )
                    .await;
            }

            self.drain_steering();
            // Mid-run notices (bg jobs, later MCP deltas) land before this sample.
            // The opening human turn already drained in `prompt_user`.
            self.drain_notifications();
            self.maybe_compact_before_sample(provider).await;
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

            let remind_batch_exploration = batch_exploration_threshold
                .is_some_and(|threshold| batch_exploration.take_reminder(threshold));
            if remind_batch_exploration {
                self.record_trace(TraceEvent::BatchExploration {
                    ts_ms: now_ms(),
                    run_id: run_id.clone(),
                    turn,
                    batch_size: 0,
                    reminder: true,
                });
            }
            let mut request_messages = self.messages.clone();
            if remind_batch_exploration {
                request_messages.push(AgentMessage::User(UserMessage {
                    content: UserContent::Text(crate::reminder::system_reminder(
                        BATCH_EXPLORATION_REMINDER,
                    )),
                    timestamp: now_ms(),
                    kind: None,
                }));
            }
            let request = CompletionRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: request_messages,
                tools: self.tool_definitions(),
                server_tools: if self.config.server_search {
                    provider.server_tools()
                } else {
                    Vec::new()
                },
                thinking_level: self.config.thinking_level,
            };

            // Always record the messages actually sent to the model (system +
            // full conversation). Tool results are size-bounded inside the helper.
            let input_preview = crate::trace::llm_input_preview(
                &request.system_prompt,
                &request.messages,
                self.llm_preview_limit(),
            );
            let context_projection_hash = messages_fingerprint(&request.messages);
            let context_projection_id = format!(
                "ctx_{}",
                context_projection_hash
                    .trim_start_matches("fp1:")
                    .replace(':', "_")
            );
            // Helper: open a generation span. Re-emitted after empty/provider retries so
            // Langfuse keeps a separate generation per sample attempt.
            let record_llm_request = |this: &Self, run_id: &str, turn: usize| {
                this.record_trace(TraceEvent::LlmRequest {
                    ts_ms: now_ms(),
                    run_id: run_id.to_string(),
                    turn,
                    message_count: request.messages.len(),
                    tools_n: request.tools.len(),
                    system_prompt_len: request.system_prompt.len(),
                    context_projection_id: Some(context_projection_id.clone()),
                    context_projection_hash: Some(context_projection_hash.clone()),
                    input_preview: input_preview.clone(),
                });
            };
            record_llm_request(self, &run_id, turn);

            let llm_start = Instant::now();
            let ttft_ms: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
            // Sample (and re-sample on empty) until we get a usable completion.
            // Empty = no visible text and no tool calls (reasoning-only counts
            // as empty — Grok Build EmptyResponse policy).
            let empty_budget = self.config.empty_response_retries;
            let mut sample_attempt = 0usize;
            let response = loop {
                sample_attempt += 1;
                let sample = {
                    let listeners: Vec<_> = self.listeners.iter().collect();
                    let ttft = ttft_ms.clone();
                    let llm_start_for_cb = llm_start;
                    let mut on_stream = |event| {
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
                            crate::streaming::StreamEvent::ServerTool { tool, status } => {
                                let agent_event = AgentEvent::ServerTool {
                                    provider: provider.name().to_string(),
                                    tool,
                                    status,
                                };
                                for listener in &listeners {
                                    listener(&agent_event);
                                }
                            }
                        }
                    };
                    provider
                        .complete_streaming(request.clone(), &mut on_stream, Some(&self.abort_flag))
                        .await
                };

                let response = match sample {
                    Ok(r) => r,
                    Err(err) => {
                        let err = map_provider_error(err);
                        if is_retryable_provider_error(&err) && sample_attempt <= empty_budget {
                            self.record_trace(TraceEvent::LlmResponse {
                                ts_ms: now_ms(),
                                run_id: run_id.clone(),
                                turn,
                                latency_ms: llm_start.elapsed().as_millis() as u64,
                                ttft_ms: *ttft_ms.lock().expect("ttft"),
                                stop_reason: "provider_retry".into(),
                                tool_calls_n: 0,
                                text_len: 0,
                                thinking_len: 0,
                                usage: TokenUsage::default(),
                                provider: provider.name().to_string(),
                                model: provider.model().to_string(),
                                output_preview: Some(format!(
                                    "{} — retry {sample_attempt}/{empty_budget}",
                                    retry_reason(&err)
                                )),
                                tool_calls: vec![],
                            });
                            if !self
                                .wait_for_completion_retry(
                                    sample_attempt,
                                    empty_budget,
                                    retry_reason(&err),
                                )
                                .await
                            {
                                return self
                                    .finish_aborted(
                                        start_len,
                                        &run_id,
                                        wall_start,
                                        turns_done,
                                        usage_at_run_start,
                                    )
                                    .await;
                            }
                            *ttft_ms.lock().expect("ttft") = None;
                            // Open a fresh generation for the next sample attempt.
                            record_llm_request(self, &run_id, turn);
                            continue;
                        }
                        self.record_trace(TraceEvent::RunEnd {
                            ts_ms: now_ms(),
                            run_id: run_id.clone(),
                            status: TraceRunStatus::Error,
                            turns: turns_done,
                            wall_ms: wall_start.elapsed().as_millis() as u64,
                            usage: self.token_usage.saturating_sub(&usage_at_run_start),
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

                // Abort is terminal — do not treat as empty or retry.
                if self.is_aborted() || response.stop_reason == StopReason::Aborted {
                    break response;
                }

                if !completion_is_empty(&response) {
                    break response;
                }

                // Empty completion: retry within budget, then fail loudly.
                if sample_attempt <= empty_budget {
                    self.record_trace(TraceEvent::LlmResponse {
                        ts_ms: now_ms(),
                        run_id: run_id.clone(),
                        turn,
                        latency_ms: llm_start.elapsed().as_millis() as u64,
                        ttft_ms: *ttft_ms.lock().expect("ttft"),
                        stop_reason: "empty_retry".into(),
                        tool_calls_n: 0,
                        text_len: 0,
                        thinking_len: extract_thinking_len(&response.content),
                        usage: response.usage,
                        provider: response.provider.clone(),
                        model: response.model.clone(),
                        output_preview: Some(format!(
                            "empty response — retry {sample_attempt}/{empty_budget}"
                        )),
                        tool_calls: vec![],
                    });
                    if !response.usage.is_zero() {
                        self.token_usage.add_assign(&response.usage);
                    }
                    if !self
                        .wait_for_completion_retry(
                            sample_attempt,
                            empty_budget,
                            "empty model response",
                        )
                        .await
                    {
                        return self
                            .finish_aborted(
                                start_len,
                                &run_id,
                                wall_start,
                                turns_done,
                                usage_at_run_start,
                            )
                            .await;
                    }
                    // Reset TTFT so the next sample can measure fresh.
                    *ttft_ms.lock().expect("ttft") = None;
                    // Open a fresh generation for the next sample attempt.
                    record_llm_request(self, &run_id, turn);
                    continue;
                }

                let err = OneError::EmptyResponse {
                    attempts: sample_attempt,
                };
                self.record_trace(TraceEvent::RunEnd {
                    ts_ms: now_ms(),
                    run_id: run_id.clone(),
                    status: TraceRunStatus::Error,
                    turns: turns_done,
                    wall_ms: wall_start.elapsed().as_millis() as u64,
                    usage: self.token_usage.saturating_sub(&usage_at_run_start),
                    final_text_len: None,
                    final_text_preview: None,
                    error: Some(err.to_string()),
                });
                self.is_busy = false;
                if let Some(hooks) = &self.hooks {
                    hooks.on_agent_end().await;
                }
                return Err(err);
            };

            let latency_ms = llm_start.elapsed().as_millis() as u64;
            let ttft = *ttft_ms.lock().expect("ttft");
            let tool_calls = extract_tool_calls(&response.content);
            if batch_exploration_threshold.is_some() {
                if batch_exploration.reminded && !tool_calls.is_empty() {
                    self.record_trace(TraceEvent::BatchExploration {
                        ts_ms: now_ms(),
                        run_id: run_id.clone(),
                        turn,
                        batch_size: tool_calls.len(),
                        reminder: false,
                    });
                }
                batch_exploration.observe(&tool_calls);
            }
            let text = extract_text(&response.content);
            let text_len = text.len();
            let thinking_len = extract_thinking_len(&response.content);
            let thinking = extract_thinking(&response.content);
            // Structured assistant message JSON (role/content/thinking/tool_calls).
            let output_preview = crate::trace::llm_output_preview(
                &text,
                &tool_calls,
                thinking.as_deref(),
                self.llm_preview_limit(),
            );
            let tool_calls_trace =
                crate::trace::trace_tool_calls(&tool_calls, self.llm_preview_limit());

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
                tool_calls: tool_calls_trace,
            });

            if !response.usage.is_zero() {
                self.token_usage.add_assign(&response.usage);
                let ctx = response.usage.context_size_tokens();
                if ctx > 0 {
                    self.last_prompt_tokens = ctx;
                }
                self.emit(AgentEvent::UsageUpdate {
                    usage: self.token_usage,
                    context_tokens: self.last_prompt_tokens,
                });
            }

            turns_done = turn + 1;

            if let Some(config) = &mut self.config.compaction_config {
                if matches!(
                    config.suppression,
                    crate::compaction::CompactionSuppression::StickyUntilSuccess
                        | crate::compaction::CompactionSuppression::Turn
                ) {
                    config.suppression = crate::compaction::CompactionSuppression::None;
                }
            }

            if self.is_aborted() || response.stop_reason == StopReason::Aborted {
                let assistant = AgentMessage::Assistant(AssistantMessage {
                    content: response.content.clone(),
                    provider: response.provider.clone(),
                    model: response.model.clone(),
                    stop_reason: StopReason::Aborted,
                    timestamp: crate::message::now_ms(),
                    citations: response.citations.clone(),
                });
                self.push_message(assistant);
                return self
                    .finish_aborted(
                        start_len,
                        &run_id,
                        wall_start,
                        turns_done,
                        usage_at_run_start,
                    )
                    .await;
            }

            let assistant = AgentMessage::Assistant(AssistantMessage {
                content: response.content.clone(),
                provider: response.provider.clone(),
                model: response.model.clone(),
                stop_reason: response.stop_reason,
                timestamp: crate::message::now_ms(),
                citations: response.citations.clone(),
            });
            self.push_message(assistant.clone());

            let mut tool_results = Vec::new();

            if tool_calls.is_empty() {
                final_text = extract_text(&response.content);
                if let Some(hooks) = self.hooks.clone() {
                    let decision = hooks.on_stop(turn, Some(&final_text)).await;
                    match decision {
                        StopDecision::Block { reason }
                            if stop_continuations < MAX_STOP_CONTINUATIONS =>
                        {
                            stop_continuations += 1;
                            self.push_message(AgentMessage::User(UserMessage {
                                content: UserContent::Text(format!(
                                    "[Stop Hook Feedback] {reason}"
                                )),
                                timestamp: crate::message::now_ms(),
                                kind: None,
                            }));
                            self.emit(AgentEvent::TurnEnd {
                                turn,
                                assistant,
                                tool_results,
                            });
                            hooks.on_turn_end(turn).await;
                            continue;
                        }
                        StopDecision::ForceStop { .. }
                        | StopDecision::Allow
                        | StopDecision::Block { .. } => {}
                    }
                }
                self.emit(AgentEvent::TurnEnd {
                    turn,
                    assistant,
                    tool_results,
                });
                if let Some(hooks) = &self.hooks {
                    hooks.on_turn_end(turn).await;
                }
                if self.drain_followup() {
                    batch_exploration = BatchExplorationState::default();
                    continue;
                }
                // Check if ReAct is waiting on background tasks
                let maybe_interest = {
                    self.wait_interest
                        .lock()
                        .expect("wait_interest lock")
                        .clone()
                };
                if let Some(mut interest) = maybe_interest {
                    if !interest.is_satisfied() {
                        let park_result = react::park_wait_interest(self, &mut interest).await;
                        match park_result {
                            react::ParkOutcome::Satisfied => {
                                self.clear_wait_interest();
                                batch_exploration = BatchExplorationState::default();
                                continue;
                            }
                            react::ParkOutcome::NewInput => {
                                // Steer/followup supersedes the declared wait: the
                                // model can re-declare with wait_tasks if still
                                // needed. Leaving the interest set would re-park on
                                // the next stop (stale waiting state).
                                self.clear_wait_interest();
                                self.drain_steering();
                                self.drain_followup();
                                batch_exploration = BatchExplorationState::default();
                                continue;
                            }
                            react::ParkOutcome::Aborted => {
                                self.clear_wait_interest();
                                return self
                                    .finish_aborted(
                                        start_len,
                                        &run_id,
                                        wall_start,
                                        turns_done,
                                        usage_at_run_start,
                                    )
                                    .await;
                            }
                        }
                    } else {
                        self.clear_wait_interest();
                    }
                }
                self.clear_wait_interest();
                self.is_busy = false;
                self.emit(AgentEvent::AgentEnd {
                    new_messages: self.new_messages_since(start_len),
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
                    usage: self.token_usage.saturating_sub(&usage_at_run_start),
                    final_text_len: Some(final_text.len()),
                    final_text_preview,
                    error: None,
                });
                return Ok(final_text);
            }

            // Gate sequentially (HITL Ask is single-slot), then run allowed tools
            // concurrently. Steer/abort mid-batch → synthetic error toolResults so
            // tool_call / tool_result pairs stay valid for the provider.
            let batch_outcome = self
                .run_tool_batch(&tool_calls, turn, &run_id, &mut tool_results)
                .await;
            match batch_outcome {
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
                        .finish_aborted(
                            start_len,
                            &run_id,
                            wall_start,
                            turns_done,
                            usage_at_run_start,
                        )
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

            // If a tool (e.g. wait_tasks) registered an active wait interest, park ReAct
            // immediately after the tool batch is complete and results are recorded,
            // before calling the LLM again.
            let maybe_interest = {
                self.wait_interest
                    .lock()
                    .expect("wait_interest lock")
                    .clone()
            };
            if let Some(mut interest) = maybe_interest {
                if !interest.is_satisfied() {
                    let park_result = react::park_wait_interest(self, &mut interest).await;
                    match park_result {
                        react::ParkOutcome::Satisfied => {
                            self.clear_wait_interest();
                            batch_exploration = BatchExplorationState::default();
                        }
                        react::ParkOutcome::NewInput => {
                            // Superseded by steer/followup — do not re-park later.
                            self.clear_wait_interest();
                            self.drain_steering();
                            self.drain_followup();
                            batch_exploration = BatchExplorationState::default();
                        }
                        react::ParkOutcome::Aborted => {
                            self.clear_wait_interest();
                            return self
                                .finish_aborted(
                                    start_len,
                                    &run_id,
                                    wall_start,
                                    turns_done,
                                    usage_at_run_start,
                                )
                                .await;
                        }
                    }
                } else {
                    self.clear_wait_interest();
                }
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
            usage: self.token_usage.saturating_sub(&usage_at_run_start),
            final_text_len: None,
            final_text_preview: None,
            error: Some(format!("max turns ({})", self.config.max_turns)),
        });
        self.emit(AgentEvent::AgentEnd {
            new_messages: self.new_messages_since(start_len),
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
        usage_at_run_start: TokenUsage,
    ) -> Result<String> {
        self.clear_wait_interest();
        self.is_busy = false;
        self.emit(AgentEvent::AgentEnd {
            new_messages: self.new_messages_since(start_len),
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
            usage: self.token_usage.saturating_sub(&usage_at_run_start),
            final_text_len: None,
            final_text_preview: None,
            error: Some("aborted".into()),
        });
        Err(OneError::Aborted)
    }

    async fn maybe_compact_before_sample(&mut self, provider: &dyn LlmProvider) {
        let Some(config) = self.config.compaction_config.clone() else {
            return;
        };
        let observed = (self.last_prompt_tokens > 0).then_some(self.last_prompt_tokens);
        let tokens = tokens_for_compaction(&self.messages, observed);
        if !should_compact_tokens(tokens, &config) {
            return;
        }

        let (older, kept) = {
            let Some((older_slice, kept_slice)) =
                split_for_compaction_forced(&self.messages, &config, true)
            else {
                return;
            };
            if older_slice.is_empty() {
                return;
            }
            (older_slice.to_vec(), kept_slice.to_vec())
        };

        self.emit(AgentEvent::CompactionStart);

        let forked_summary = if can_fork_summarize_prefix(&older) {
            provider
                .complete(CompletionRequest {
                    system_prompt: self.config.system_prompt.clone(),
                    messages: compaction_request_messages(&older, None),
                    tools: self.tool_definitions(),
                    server_tools: if self.config.server_search {
                        provider.server_tools()
                    } else {
                        Vec::new()
                    },
                    thinking_level: ThinkingLevel::Off,
                })
                .await
                .ok()
                .map(|resp| extract_text(&resp.content).trim().to_string())
                .filter(|text| !text.is_empty())
        } else {
            None
        };

        let summary = match forked_summary {
            Some(text) => text,
            None => {
                let prompt = summarization_prompt(&older, None);
                match provider
                    .complete(CompletionRequest {
                        system_prompt:
                            "You summarize coding-agent conversations for context compaction."
                                .into(),
                        messages: vec![AgentMessage::user_text(prompt)],
                        tools: Vec::new(),
                        server_tools: Vec::new(),
                        thinking_level: ThinkingLevel::Off,
                    })
                    .await
                {
                    Ok(response) => {
                        let text = extract_text(&response.content).trim().to_string();
                        if text.is_empty() {
                            extractive_summary(&older, config.max_summary_chars)
                        } else {
                            text
                        }
                    }
                    Err(_error) => extractive_summary(&older, config.max_summary_chars),
                }
            }
        };
        if summary.trim().is_empty() {
            return;
        }

        let tokens_after =
            tokens_for_compaction(&compacted_live_messages(&summary, kept.clone()), None);
        self.messages = compacted_live_messages(&summary, kept);
        self.last_prompt_tokens = 0;
        self.record_context_projection("compaction", Vec::new());
        if let Some(config) = &mut self.config.compaction_config {
            config.suppression = crate::compaction::CompactionSuppression::StickyUntilSuccess;
        }
        let kept_turns = crate::compaction::user_turn_count(&self.messages[1..]);
        self.intra_turn_compaction = Some(IntraTurnCompaction {
            summary,
            applied: CompactApplied {
                tokens_before: tokens as u64,
                tokens_after: tokens_after as u64,
                trigger: CompactTrigger::Auto,
                kept_turns,
            },
        });
        self.emit(AgentEvent::CompactionEnd {
            tokens_before: tokens as u64,
            tokens_after: tokens_after as u64,
            kept_turns,
        });
    }

    fn drain_steering(&mut self) {
        let mut queue = self.steering_queue.lock().expect("steering queue lock");
        // Preserve FIFO order (push to end, drain from front).
        let items: Vec<_> = queue.drain(..).collect();
        drop(queue);
        for text in items {
            // Steer keeps role=user for providers but is tagged `kind="steer"`
            // so transcript/UI never mistakes it for a new user turn.
            let msg = AgentMessage::steer_text(text);
            if let AgentMessage::User(u) = &msg {
                self.emit(AgentEvent::SteerApplied {
                    text: u.content.as_plain_text(),
                });
            }
            self.push_message(msg);
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
            let text = crate::reminder::system_reminder(text);
            self.push_message(AgentMessage::user_text(text));
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
            self.push_message(AgentMessage::user_text(text));
        }
        true
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
        // Map cross-agent / hallucinated names before gate + dispatch.
        let canonical = resolve_tool_name(&effective.name);
        if canonical != effective.name {
            effective.name = canonical.to_string();
        }
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

        match self.tools.iter().find(|tool| {
            let def_name = tool.definition().name;
            def_name == effective.name || resolve_tool_name(&def_name) == effective.name
        }) {
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
            ToolExecutionResult {
                output: ToolOutput::text(reason),
                is_error: true,
                gate_decision: None,
                duration_ms: 0,
            },
            tool_results,
        );
    }

    /// Trace + UI event for a finished tool (does not push agent ToolResult yet).
    fn emit_tool_end(
        &mut self,
        call: &ToolCall,
        turn: usize,
        run_id: &str,
        execution: &ToolExecutionResult,
    ) {
        let output_text = execution.output.as_text();
        let output_bytes = output_text.len();
        // Same as generation: short preview by default; --trace-full expands budget.
        let output_preview = crate::trace::text_preview(&output_text, self.preview_limit());
        self.record_trace(TraceEvent::ToolEnd {
            ts_ms: now_ms(),
            run_id: run_id.to_string(),
            turn,
            call_id: call.id.clone(),
            name: call.name.clone(),
            duration_ms: execution.duration_ms,
            is_error: execution.is_error,
            output_bytes,
            gate: execution.gate_decision.clone(),
            output_preview,
        });
        self.emit(AgentEvent::ToolExecutionEnd {
            tool_call: call.clone(),
            output: execution.output.clone(),
            is_error: execution.is_error,
        });
    }

    /// Push ToolResult into agent messages / turn results (call order preserved by caller).
    fn record_tool_result(
        &mut self,
        call: &ToolCall,
        execution: ToolExecutionResult,
        tool_results: &mut Vec<AgentMessage>,
    ) {
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: execution.output.content.clone(),
            is_error: execution.is_error,
            timestamp: crate::message::now_ms(),
        });
        self.push_message(result.clone());
        tool_results.push(result);
    }

    /// Emit UI/trace end and record ToolResult (synthetic skips, error paths).
    fn finish_tool_result(
        &mut self,
        call: &ToolCall,
        turn: usize,
        run_id: &str,
        execution: ToolExecutionResult,
        tool_results: &mut Vec<AgentMessage>,
    ) {
        self.emit_tool_end(call, turn, run_id, &execution);
        self.record_tool_result(call, execution, tool_results);
    }

    fn emit(&self, event: AgentEvent) {
        for listener in &self.listeners {
            listener(&event);
        }
    }

    /// Wait between completion attempts while still allowing Esc to abort.
    async fn wait_for_completion_retry(
        &mut self,
        retry: usize,
        max_retries: usize,
        reason: &str,
    ) -> bool {
        let delay = retry_backoff_delay(retry);
        self.emit(AgentEvent::RetryScheduled {
            retry,
            max_retries,
            delay,
            reason: reason.to_string(),
        });
        if crate::streaming::race_abort(tokio::time::sleep(delay), Some(&self.abort_flag))
            .await
            .is_err()
        {
            return false;
        }
        self.emit(AgentEvent::RetryStarted { retry, max_retries });
        true
    }
}

struct ToolExecutionResult {
    output: ToolOutput,
    is_error: bool,
    gate_decision: Option<TraceGateDecision>,
    duration_ms: u64,
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
/// Everything else (writes, shell, destructive MCP, ask_user, plan tools, unknown names) runs serially.
pub fn is_parallel_safe_tool(name: &str) -> bool {
    let resolved = resolve_tool_name(name);
    matches!(
        resolved,
        // `task` is explore-only (read-only research) in MVP → concurrent-safe.
        // When general/write subagents land, keep them serial via a different
        // name or gate classification on mode.
        "read"
            | "grep"
            | "ls"
            | "bash_output"
            | "web_search"
            | "web_fetch"
            | "task"
            | "memory_search"
            | "search_tool"
            | "mcp_status"
            | "status"
    ) || resolved.starts_with("deepwiki__read_")
        || resolved.starts_with("deepwiki__ask_")
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
            // Foreground bash titles: "exit N" (ok) or "command failed (exit N|signal)".
            if let Some(rest) = text.strip_prefix("exit ") {
                let code = rest.split(|c: char| c.is_whitespace()).next().unwrap_or("");
                if code == "signal" {
                    return true;
                }
                if let Ok(n) = code.parse::<i64>() {
                    return n != 0;
                }
            }
            if text.starts_with("command failed (") {
                return true;
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

/// Delay before the one-based `retry` attempt. Fibonacci-like growth gives an
/// overloaded provider breathing room without making early recovery sluggish.
pub fn retry_backoff_delay(retry: usize) -> Duration {
    let index = retry.saturating_sub(1).min(RETRY_BACKOFF_SECS.len() - 1);
    Duration::from_secs(RETRY_BACKOFF_SECS[index])
}

/// Whether the provider failure is likely temporary and safe to retry.
///
/// We deliberately do not retry auth, malformed-request, model-not-found, or
/// context-overflow failures; those need user/configuration action instead.
pub fn is_retryable_provider_error(err: &OneError) -> bool {
    let OneError::Provider(message) = err else {
        return false;
    };
    let message = message.to_ascii_lowercase();

    // Any 500 or 5xx server error is directly retryable.
    if message.contains("500")
        || message.contains("502")
        || message.contains("503")
        || message.contains("504")
        || message.contains("529")
    {
        return true;
    }

    [
        "at capacity",
        "capacity due to high demand",
        "overloaded",
        "upstream request failed",
        "upstream",
        "rate limit",
        "rate_limit",
        "too many requests",
        "resource has been exhausted",
        "quota exceeded",
        "429",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "server error",
        "internal error",
        "timeout",
        "timed out",
        "deadline exceeded",
        "temporarily unavailable",
        "connection reset",
        "connection refused",
        "broken pipe",
        "network error",
        "fetch failed",
        "error decoding response body",
        "decoding response body",
        "connection closed",
        "stream error",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn retry_reason(err: &OneError) -> &'static str {
    let OneError::Provider(message) = err else {
        return "provider request failed";
    };
    let message = message.to_ascii_lowercase();
    if message.contains("500") || message.contains("internal server error") {
        "provider 500 server error"
    } else if message.contains("capacity")
        || message.contains("overloaded")
        || message.contains("529")
    {
        "provider at capacity"
    } else if message.contains("rate limit") || message.contains("429") || message.contains("quota")
    {
        "provider rate limited"
    } else if message.contains("timeout")
        || message.contains("timed out")
        || message.contains("deadline")
    {
        "request timed out"
    } else if message.contains("decoding response body")
        || message.contains("connection closed")
        || message.contains("stream error")
    {
        "stream interrupted"
    } else {
        "temporary upstream failure"
    }
}

pub fn extract_tool_calls(content: &[ContentBlock]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some(ToolCall {
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

/// True when the model produced nothing actionable for the agent loop.
///
/// - Tool calls → not empty (even with blank text).
/// - Non-empty text → not empty.
/// - Reasoning/thinking only → **empty** (Grok Build `ReasoningOnly` policy:
///   re-sample rather than end the turn with no user-visible action).
/// - Completely blank content → empty.
pub fn completion_is_empty(response: &CompletionResponse) -> bool {
    if response
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    {
        return false;
    }
    extract_text(&response.content).trim().is_empty()
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

/// Concatenate thinking blocks for generation observation output (may be truncated later).
fn extract_thinking(content: &[ContentBlock]) -> Option<String> {
    let parts: Vec<&str> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                Some(thinking.as_str())
            }
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
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
    use crate::message::TextOrImage;
    use crate::tool::ToolDefinition;

    struct BatchTestTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for BatchTestTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: "test tool".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        async fn execute(&self, _call: &ToolCall) -> Result<ToolOutput> {
            Ok(ToolOutput::text("ok"))
        }
    }

    struct BatchTestProvider {
        provider: &'static str,
        model: &'static str,
        batches: Vec<Vec<&'static str>>,
        requests: Mutex<Vec<CompletionRequest>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for BatchTestProvider {
        fn name(&self) -> &str {
            self.provider
        }
        fn model(&self) -> &str {
            self.model
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            let calls = self.batches.get(turn).cloned().unwrap_or_default();
            let content = if calls.is_empty() {
                vec![ContentBlock::text("done")]
            } else {
                calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| ContentBlock::ToolCall {
                        id: format!("{turn}-{i}"),
                        name: name.into(),
                        arguments: serde_json::json!({}),
                    })
                    .collect()
            };
            let stop_reason = if content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
            Ok(CompletionResponse {
                provider: self.provider.into(),
                model: self.model.into(),
                content,
                stop_reason,
                usage: TokenUsage::default(),
                citations: Vec::new(),
            })
        }
    }

    async fn run_batch_test(
        provider_name: &'static str,
        model: &'static str,
        level: ThinkingLevel,
        batches: Vec<Vec<&'static str>>,
    ) -> (Vec<CompletionRequest>, Vec<TraceEvent>) {
        run_batch_test_with_rules(
            provider_name,
            model,
            level,
            batches,
            vec![BatchExplorationRule {
                provider: "cpa".into(),
                model: "gemini-3.8-flash-high".into(),
                thinking_level: Some(ThinkingLevel::Medium),
                after_single_reads: 4,
            }],
        )
        .await
    }

    async fn run_batch_test_with_rules(
        provider_name: &'static str,
        model: &'static str,
        level: ThinkingLevel,
        batches: Vec<Vec<&'static str>>,
        rules: Vec<BatchExplorationRule>,
    ) -> (Vec<CompletionRequest>, Vec<TraceEvent>) {
        let provider = BatchTestProvider {
            provider: provider_name,
            model,
            batches,
            requests: Mutex::new(Vec::new()),
        };
        let trace = Arc::new(crate::trace::MemoryTrace::new());
        let tools: Vec<Arc<dyn Tool>> = ["read", "grep", "ls", "write"]
            .into_iter()
            .map(|name| Arc::new(BatchTestTool(name)) as Arc<dyn Tool>)
            .collect();
        let mut agent = Agent::new(
            AgentConfig {
                thinking_level: level,
                batch_exploration: rules,
                ..AgentConfig::default()
            },
            tools,
        );
        agent.set_trace(Some(trace.clone()));
        assert_eq!(agent.prompt(&provider, "explore").await.unwrap(), "done");
        let requests = provider.requests.into_inner().unwrap();
        (requests, trace.events())
    }

    fn reminder_count(request: &CompletionRequest) -> usize {
        request
            .messages
            .iter()
            .filter(|message| {
                matches!(message,
                    AgentMessage::User(UserMessage { content: UserContent::Text(text), .. })
                        if text.contains(BATCH_EXPLORATION_REMINDER)
                            && crate::reminder::has_system_reminder(text)
                )
            })
            .count()
    }

    #[tokio::test]
    async fn batch_exploration_reminds_once_and_tracks_following_batches() {
        let (requests, events) = run_batch_test(
            "cpa",
            "gemini-3.8-flash-high",
            ThinkingLevel::Medium,
            vec![
                vec!["read"],
                vec!["grep"],
                vec!["read"],
                vec!["ls"],
                vec!["read"],
                vec!["read", "grep"],
                vec!["read"],
            ],
        )
        .await;
        assert_eq!(
            requests.iter().map(reminder_count).collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 1, 0, 0, 0]
        );
        let observations: Vec<(usize, usize, bool)> = events
            .iter()
            .filter_map(|event| {
                if let TraceEvent::BatchExploration {
                    turn,
                    batch_size,
                    reminder,
                    ..
                } = event
                {
                    Some((*turn, *batch_size, *reminder))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            observations,
            vec![(4, 0, true), (4, 1, false), (5, 2, false), (6, 1, false)]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TraceEvent::ToolEnd { .. }))
                .count(),
            8,
            "single dependent checks must still execute after the reminder"
        );
    }

    #[tokio::test]
    async fn batch_exploration_resets_on_multi_call_and_write() {
        for interruption in [vec!["read", "grep"], vec!["write"]] {
            let (requests, _) = run_batch_test(
                "cpa",
                "gemini-3.8-flash-high",
                ThinkingLevel::Medium,
                vec![
                    vec!["read"],
                    vec!["read"],
                    interruption,
                    vec!["read"],
                    vec!["read"],
                    vec!["read"],
                    vec!["read"],
                ],
            )
            .await;
            assert_eq!(
                requests.iter().map(reminder_count).collect::<Vec<_>>(),
                vec![0, 0, 0, 0, 0, 0, 0, 1]
            );
        }
    }

    #[tokio::test]
    async fn batch_exploration_is_off_for_other_model_or_thinking_level() {
        for (provider, model, level) in [
            ("other", "gemini-3.8-flash-high", ThinkingLevel::Medium),
            ("cpa", "gemini-3.8-flash-low", ThinkingLevel::Medium),
            ("cpa", "gemini-3.8-flash-high", ThinkingLevel::High),
        ] {
            let (requests, events) =
                run_batch_test(provider, model, level, vec![vec!["read"]; 5]).await;
            assert!(requests.iter().all(|request| reminder_count(request) == 0));
            assert!(!events
                .iter()
                .any(|event| matches!(event, TraceEvent::BatchExploration { .. })));
        }
        let (requests, events) = run_batch_test_with_rules(
            "cpa",
            "gemini-3.8-flash-high",
            ThinkingLevel::Medium,
            vec![vec!["read"]; 5],
            Vec::new(),
        )
        .await;
        assert!(requests.iter().all(|request| reminder_count(request) == 0));
        assert!(!events
            .iter()
            .any(|event| matches!(event, TraceEvent::BatchExploration { .. })));
    }

    #[tokio::test]
    async fn batch_exploration_matches_parenthetical_model_suffix() {
        let (requests, _) = run_batch_test_with_rules(
            "cpa",
            "gemini-3.8-flash-high(medium)",
            ThinkingLevel::High,
            vec![vec!["read"]; 5],
            vec![BatchExplorationRule {
                provider: "cpa".into(),
                model: "gemini-3.8-flash-high".into(),
                thinking_level: None,
                after_single_reads: 4,
            }],
        )
        .await;
        assert_eq!(
            requests.iter().map(reminder_count).collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 1, 0]
        );

        let (requests, _) = run_batch_test_with_rules(
            "cpa",
            "gemini-3.8-flash-highest",
            ThinkingLevel::High,
            vec![vec!["read"]; 5],
            vec![BatchExplorationRule {
                provider: "cpa".into(),
                model: "gemini-3.8-flash-high".into(),
                thinking_level: None,
                after_single_reads: 4,
            }],
        )
        .await;
        assert!(requests.iter().all(|request| reminder_count(request) == 0));
    }

    #[tokio::test]
    async fn batch_exploration_uses_configured_model_and_threshold() {
        let (requests, _) = run_batch_test_with_rules(
            "custom",
            "model-x",
            ThinkingLevel::Low,
            vec![vec!["read"], vec!["grep"], vec!["read"]],
            vec![BatchExplorationRule {
                provider: "custom".into(),
                model: "model-x".into(),
                thinking_level: None,
                after_single_reads: 2,
            }],
        )
        .await;
        assert_eq!(
            requests.iter().map(reminder_count).collect::<Vec<_>>(),
            vec![0, 0, 1, 0]
        );
    }

    #[test]
    fn system_prompt_uses_one_tool_names() {
        assert!(
            DEFAULT_SYSTEM_PROMPT.contains("`ls` over `bash ls`"),
            "tool policy should steer toward the ls tool"
        );
        assert!(
            DEFAULT_SYSTEM_PROMPT.contains("line counts"),
            "tool policy should mention ls line counts"
        );
        assert!(
            DEFAULT_SYSTEM_PROMPT.contains("markdown tables"),
            "formatting should steer structured output toward GFM tables"
        );
        assert!(
            !DEFAULT_SYSTEM_PROMPT.contains("`read_file`"),
            "Grok tool names must not leak into One's system prompt"
        );
        assert!(
            !DEFAULT_SYSTEM_PROMPT.contains("`search_replace`"),
            "Grok tool names must not leak into One's system prompt"
        );
    }

    #[test]
    fn parallel_safe_tools_are_read_only() {
        assert!(is_parallel_safe_tool("read"));
        assert!(is_parallel_safe_tool("grep"));
        assert!(is_parallel_safe_tool("ls"));
        assert!(is_parallel_safe_tool("web_search"));
        assert!(is_parallel_safe_tool("task")); // explore MVP concurrent
        assert!(is_parallel_safe_tool("memory_search"));
        assert!(is_parallel_safe_tool("search_tool"));
        assert!(is_parallel_safe_tool("mcp_status"));
        assert!(is_parallel_safe_tool("status"));
        assert!(is_parallel_safe_tool("deepwiki__read_wiki_structure"));
        assert!(is_parallel_safe_tool("deepwiki__ask_question"));
        assert!(!is_parallel_safe_tool("write"));
        assert!(!is_parallel_safe_tool("edit"));
        assert!(!is_parallel_safe_tool("bash"));
        assert!(!is_parallel_safe_tool("ask_user"));
        assert!(!is_parallel_safe_tool("mcp_something"));
        assert!(!is_parallel_safe_tool("exit_plan_mode"));
    }

    /// Fast + slow parallel-safe tools must each emit ToolExecutionEnd when *they*
    /// finish — not only after the whole batch drains (UI spinner bug).
    #[tokio::test]
    async fn parallel_tools_emit_end_as_each_finishes() {
        use crate::tool::{Tool, ToolDefinition};
        use std::time::Duration;

        struct DelayTool {
            name: String,
            delay: Duration,
        }

        #[async_trait::async_trait]
        impl Tool for DelayTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: self.name.clone(),
                    description: "delayed".into(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }

            async fn execute(&self, _call: &ToolCall) -> Result<ToolOutput> {
                tokio::time::sleep(self.delay).await;
                Ok(ToolOutput::text(format!("{}-ok", self.name)))
            }
        }

        struct TwoToolsProvider {
            calls: AtomicU64,
        }

        #[async_trait::async_trait]
        impl LlmProvider for TwoToolsProvider {
            fn name(&self) -> &str {
                "parallel-end-test"
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
                        content: vec![
                            ContentBlock::ToolCall {
                                id: "fast".into(),
                                name: "ls".into(),
                                arguments: serde_json::json!({}),
                            },
                            ContentBlock::ToolCall {
                                id: "slow".into(),
                                name: "find".into(),
                                arguments: serde_json::json!({}),
                            },
                        ],
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().to_string(),
                        model: self.model().to_string(),
                        content: vec![ContentBlock::Text {
                            text: "done".into(),
                        }],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let ends: Arc<Mutex<Vec<(String, std::time::Instant)>>> = Arc::new(Mutex::new(Vec::new()));
        let ends_l = ends.clone();
        let mut agent = Agent::new(
            AgentConfig::default(),
            vec![
                Arc::new(DelayTool {
                    name: "ls".into(),
                    delay: Duration::from_millis(20),
                }),
                Arc::new(DelayTool {
                    name: "find".into(),
                    delay: Duration::from_millis(200),
                }),
            ],
        );
        agent.subscribe(Box::new(move |ev| {
            if let AgentEvent::ToolExecutionEnd { tool_call, .. } = ev {
                ends_l
                    .lock()
                    .expect("ends")
                    .push((tool_call.id.clone(), std::time::Instant::now()));
            }
        }));

        let t0 = std::time::Instant::now();
        agent
            .prompt(
                &TwoToolsProvider {
                    calls: AtomicU64::new(0),
                },
                "go",
            )
            .await
            .expect("prompt ok");

        let recorded = ends.lock().expect("ends").clone();
        assert_eq!(recorded.len(), 2, "both tools should emit ToolExecutionEnd");
        assert_eq!(recorded[0].0, "fast", "fast tool should finish first");
        assert_eq!(recorded[1].0, "slow");
        // Fast end must arrive well before slow (not batched at join_all).
        let fast_at = recorded[0].1.duration_since(t0);
        let slow_at = recorded[1].1.duration_since(t0);
        assert!(
            fast_at < Duration::from_millis(100),
            "fast ToolEnd should fire early, got {fast_at:?}"
        );
        assert!(
            slow_at >= Duration::from_millis(150),
            "slow ToolEnd should wait for its own work, got {slow_at:?}"
        );
        assert!(
            slow_at.saturating_sub(fast_at) >= Duration::from_millis(80),
            "ends must not be batched: fast={fast_at:?} slow={slow_at:?}"
        );
    }

    /// UI ToolExecutionEnd must fire before after_tool hooks so a slow/hanging
    /// post-tool extension cannot leave the parent transcript spinner stuck
    /// after the tool has already returned (e.g. foreground `task`).
    #[tokio::test]
    async fn tool_end_emits_before_after_tool_hooks() {
        use crate::tool::{Tool, ToolDefinition};
        use crate::tool_gate::{ToolGate, ToolGateDecision};
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        struct InstantTool;
        #[async_trait::async_trait]
        impl Tool for InstantTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "ls".into(),
                    description: "instant".into(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }
            async fn execute(&self, _call: &ToolCall) -> Result<ToolOutput> {
                Ok(ToolOutput::text("ok"))
            }
        }

        struct SlowAfterGate {
            after_entered: Arc<AtomicBool>,
            end_seen_before_after: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl ToolGate for SlowAfterGate {
            async fn check(&self, _call: &ToolCall) -> ToolGateDecision {
                ToolGateDecision::Allow
            }
            async fn after_tool(&self, _call: &ToolCall, _output: &ToolOutput, _is_error: bool) {
                // If ToolExecutionEnd already fired, the flag is set by the listener.
                if self.end_seen_before_after.load(Ordering::SeqCst) {
                    // good path recorded below
                }
                self.after_entered.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }

        struct OneToolProvider {
            calls: AtomicU64,
        }
        #[async_trait::async_trait]
        impl LlmProvider for OneToolProvider {
            fn name(&self) -> &str {
                "end-before-after"
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
                            name: "ls".into(),
                            arguments: serde_json::json!({}),
                        }],
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().to_string(),
                        model: self.model().to_string(),
                        content: vec![ContentBlock::Text {
                            text: "done".into(),
                        }],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let end_at: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
        let after_entered = Arc::new(AtomicBool::new(false));
        let end_seen_before_after = Arc::new(AtomicBool::new(false));
        let end_at_l = end_at.clone();
        let after_entered_l = after_entered.clone();
        let end_seen_l = end_seen_before_after.clone();

        let mut agent = Agent::new(AgentConfig::default(), vec![Arc::new(InstantTool)]);
        agent.set_tool_gate(Some(Arc::new(SlowAfterGate {
            after_entered: after_entered.clone(),
            end_seen_before_after: end_seen_before_after.clone(),
        })));
        agent.subscribe(Box::new(move |ev| {
            if let AgentEvent::ToolExecutionEnd { .. } = ev {
                // Record whether after_tool has started yet.
                if !after_entered_l.load(Ordering::SeqCst) {
                    end_seen_l.store(true, Ordering::SeqCst);
                }
                *end_at_l.lock().expect("end_at") = Some(std::time::Instant::now());
            }
        }));

        agent
            .prompt(
                &OneToolProvider {
                    calls: AtomicU64::new(0),
                },
                "go",
            )
            .await
            .expect("prompt ok");

        assert!(
            end_seen_before_after.load(Ordering::SeqCst),
            "ToolExecutionEnd must fire before after_tool begins"
        );
        assert!(
            end_at.lock().expect("end_at").is_some(),
            "ToolExecutionEnd must have fired"
        );
        // after_tool is fire-and-forget; wait briefly for the spawned hook.
        let deadline = Instant::now() + Duration::from_millis(500);
        while !after_entered.load(Ordering::SeqCst) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            after_entered.load(Ordering::SeqCst),
            "after_tool should still run after UI end"
        );
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
    fn token_usage_saturating_sub_for_run_delta() {
        let baseline = TokenUsage {
            input_tokens: 10_000,
            output_tokens: 100,
            cache_read_tokens: 50,
            cache_write_tokens: 0,
        };
        let cumulative = TokenUsage {
            input_tokens: 32_000,
            output_tokens: 250,
            cache_read_tokens: 80,
            cache_write_tokens: 10,
        };
        let delta = cumulative.saturating_sub(&baseline);
        assert_eq!(delta.input_tokens, 22_000);
        assert_eq!(delta.output_tokens, 150);
        assert_eq!(delta.cache_read_tokens, 30);
        assert_eq!(delta.cache_write_tokens, 10);
        // No underflow when baseline is higher (should not happen in practice).
        assert!(baseline.saturating_sub(&cumulative).is_zero());
    }

    #[tokio::test]
    async fn run_end_usage_is_per_run_not_session_cumulative() {
        use crate::trace::MemoryTrace;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingProvider {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for CountingProvider {
            fn name(&self) -> &str {
                "count"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: format!("reply-{n}"),
                    }],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage {
                        // Distinct per call so cumulative vs delta is obvious.
                        input_tokens: 100 * (n as u64 + 1),
                        output_tokens: 10 * (n as u64 + 1),
                        ..Default::default()
                    },
                    citations: Vec::new(),
                })
            }
        }

        let mem = Arc::new(MemoryTrace::new());
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_trace(Some(mem.clone()));
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
        };

        agent.prompt(&provider, "first").await.expect("run1");
        agent.prompt(&provider, "second").await.expect("run2");

        // Session total still accumulates for UI / RPC.
        assert_eq!(agent.token_usage.input_tokens, 100 + 200);
        assert_eq!(agent.token_usage.output_tokens, 10 + 20);

        let run_ends: Vec<_> = mem
            .events()
            .into_iter()
            .filter_map(|e| match e {
                TraceEvent::RunEnd { usage, .. } => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(run_ends.len(), 2);
        assert_eq!(run_ends[0].input_tokens, 100);
        assert_eq!(run_ends[0].output_tokens, 10);
        // Second RunEnd must be this run only — not 300/30 cumulative.
        assert_eq!(run_ends[1].input_tokens, 200);
        assert_eq!(run_ends[1].output_tokens, 20);
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
    fn context_size_tokens_anthropic_cache_hit_without_write() {
        // Pure cache hit: uncached tail << cached prefix (disjoint fields).
        let u = TokenUsage {
            input_tokens: 50,
            output_tokens: 20,
            cache_read_tokens: 12_000,
            cache_write_tokens: 0,
        };
        assert_eq!(u.context_size_tokens(), 12_050);
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
    async fn unlimited_turn_budget_continues_past_64_tool_loops() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct LoopingProvider {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for LoopingProvider {
            fn name(&self) -> &str {
                "unlimited-turn-test"
            }

            fn model(&self) -> &str {
                "test"
            }

            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let content = if call < 65 {
                    vec![ContentBlock::ToolCall {
                        id: format!("tool-{call}"),
                        name: "missing_tool".into(),
                        arguments: serde_json::json!({}),
                    }]
                } else {
                    vec![ContentBlock::Text {
                        text: "completed".into(),
                    }]
                };
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content,
                    stop_reason: if call < 65 {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    },
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let provider = LoopingProvider {
            calls: AtomicUsize::new(0),
        };
        let mut agent = Agent::new(
            AgentConfig {
                max_turns: 0,
                ..AgentConfig::default()
            },
            Vec::new(),
        );

        assert_eq!(
            agent.prompt(&provider, "finish the task").await.unwrap(),
            "completed"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 66);
    }

    #[tokio::test]
    async fn intra_turn_compaction_runs_before_the_next_model_sample() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct CompactionProvider {
            main_calls: AtomicUsize,
            summary_calls: AtomicUsize,
            saw_summary_on_second_sample: AtomicBool,
        }

        #[async_trait::async_trait]
        impl LlmProvider for CompactionProvider {
            fn name(&self) -> &str {
                "intra-turn-compaction"
            }

            fn model(&self) -> &str {
                "test"
            }

            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let is_compaction_request = request
                    .system_prompt
                    .starts_with("You summarize coding-agent")
                    || request.messages.last().is_some_and(|m| match m {
                        AgentMessage::User(u) => u
                            .content
                            .as_plain_text()
                            .contains("context compaction summary"),
                        _ => false,
                    });
                if is_compaction_request {
                    // Cache-sharing forked compaction keeps the session system prompt
                    // and replays the prior conversation prefix before the trailing prompt.
                    assert_eq!(request.system_prompt, AgentConfig::default().system_prompt);
                    assert!(request.messages.len() >= 4);
                    self.summary_calls.fetch_add(1, Ordering::SeqCst);
                    return Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("durable compact summary")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    });
                }

                let call = self.main_calls.fetch_add(1, Ordering::SeqCst);
                if call == 1 {
                    self.saw_summary_on_second_sample.store(
                        request.messages.iter().any(|message| match message {
                            AgentMessage::Assistant(assistant) => assistant.content.iter().any(|block| {
                                matches!(block, ContentBlock::Text { text }
                                    if text.contains("[Compaction summary]\ndurable compact summary"))
                            }),
                            _ => false,
                        }),
                        Ordering::SeqCst,
                    );
                }
                let content = if call == 0 {
                    vec![ContentBlock::ToolCall {
                        id: "tool-1".into(),
                        name: "missing_tool".into(),
                        arguments: serde_json::json!({}),
                    }]
                } else {
                    vec![ContentBlock::text("done")]
                };
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content,
                    stop_reason: if call == 0 {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    },
                    usage: TokenUsage {
                        input_tokens: if call == 0 { 1_000 } else { 20 },
                        ..TokenUsage::default()
                    },
                    citations: Vec::new(),
                })
            }
        }

        let provider = CompactionProvider {
            main_calls: AtomicUsize::new(0),
            summary_calls: AtomicUsize::new(0),
            saw_summary_on_second_sample: AtomicBool::new(false),
        };
        let mut config = AgentConfig::default();
        let mut compact = crate::compaction::CompactionConfig::from_context_window(10_000);
        compact.token_threshold = 500;
        config.compaction_config = Some(compact);
        let mut agent = Agent::new(config, Vec::new());
        let end_events = Arc::new(AtomicUsize::new(0));
        let end_events_clone = end_events.clone();
        agent.subscribe(Box::new(move |event| {
            if let AgentEvent::AgentEnd { new_messages } = event {
                end_events_clone.fetch_add(1, Ordering::SeqCst);
                assert!(!new_messages.is_empty());
            }
        }));

        assert_eq!(
            agent.prompt(&provider, "finish the task").await.unwrap(),
            "done"
        );
        assert_eq!(provider.main_calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider.summary_calls.load(Ordering::SeqCst), 1);
        assert!(provider.saw_summary_on_second_sample.load(Ordering::SeqCst));
        assert!(agent.intra_turn_compaction().is_some());
        assert_eq!(end_events.load(Ordering::SeqCst), 1);
        assert!(agent
            .last_run_transcript()
            .iter()
            .any(|message| matches!(message, AgentMessage::ToolResult(_))));
        assert!(!agent
            .last_run_transcript()
            .iter()
            .any(|message| match message {
                AgentMessage::Assistant(assistant) => assistant.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text }
                    if text.contains("[Compaction summary]"))
                }),
                _ => false,
            }));
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
                on_event(crate::streaming::StreamEvent::TextDelta(
                    "partial".to_string(),
                ));
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: "partial".to_string(),
                    }],
                    stop_reason: StopReason::Aborted,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
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
                        citations: Vec::new(),
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
                        citations: Vec::new(),
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

    #[test]
    fn bash_command_failed_title_is_error_without_details() {
        // Fallback when details are missing: new failure title must still count.
        let failed = ToolOutput::text(
            "command failed (exit 1)\nsandbox: bwrap · mode=workspace-write\nTraceback...",
        );
        assert!(tool_output_indicates_error("bash", &failed));

        let ok = ToolOutput::text("exit 0\nsandbox: bwrap · mode=workspace-write\nok\n");
        assert!(!tool_output_indicates_error("bash", &ok));
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
                    let users: Vec<String> = request
                        .messages
                        .iter()
                        .filter_map(|m| match m {
                            AgentMessage::User(u) => Some(u.content.as_plain_text()),
                            _ => None,
                        })
                        .collect();
                    assert!(users.len() >= 2, "notice then human query, got {users:?}");
                    assert!(
                        users[0].contains("[Background task completed]")
                            && crate::reminder::has_system_reminder(&users[0]),
                        "notification should be a tagged user message before the query: {users:?}"
                    );
                    assert!(
                        users[1].contains("<user_query>") && users[1].contains("hi"),
                        "human turn should be wrapped in <user_query> after the notice: {users:?}"
                    );
                }
                Ok(CompletionResponse {
                    provider: self.name().to_string(),
                    model: self.model().to_string(),
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.push_notification("[Background task completed]\ntask_id: bg_test_1\nexit: 0\n");
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

    fn empty_resp(content: Vec<ContentBlock>) -> CompletionResponse {
        CompletionResponse {
            provider: "test".into(),
            model: "test".into(),
            content,
            stop_reason: StopReason::Stop,
            usage: TokenUsage::default(),
            citations: Vec::new(),
        }
    }

    #[test]
    fn completion_is_empty_rules() {
        assert!(completion_is_empty(&empty_resp(vec![])));
        assert!(completion_is_empty(&empty_resp(vec![
            ContentBlock::thinking("only reasoning")
        ])));
        assert!(!completion_is_empty(&empty_resp(vec![ContentBlock::text(
            "hello"
        )])));
        assert!(!completion_is_empty(&empty_resp(vec![
            ContentBlock::thinking("reason"),
            ContentBlock::text("hi"),
        ])));
        assert!(!completion_is_empty(&empty_resp(vec![
            ContentBlock::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
            }
        ])));
        // Tool call with no text is still actionable.
        assert!(!completion_is_empty(&empty_resp(vec![
            ContentBlock::thinking("planning"),
            ContentBlock::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }
        ])));
    }

    #[tokio::test]
    async fn empty_response_retries_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct EmptyThenText {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for EmptyThenText {
            fn name(&self) -> &str {
                "empty-then-text"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Ok(empty_resp(vec![]))
                } else {
                    Ok(empty_resp(vec![ContentBlock::text("recovered")]))
                }
            }
        }

        let provider = EmptyThenText {
            calls: AtomicUsize::new(0),
        };
        let mut agent = Agent::new(
            AgentConfig {
                empty_response_retries: 2,
                ..AgentConfig::default()
            },
            Vec::new(),
        );
        let out = agent.prompt(&provider, "hi").await.expect("should recover");
        assert_eq!(out, "recovered");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn empty_response_exhausted_returns_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AlwaysEmpty {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for AlwaysEmpty {
            fn name(&self) -> &str {
                "always-empty"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(empty_resp(vec![ContentBlock::thinking("…")]))
            }
        }

        let provider = AlwaysEmpty {
            calls: AtomicUsize::new(0),
        };
        let mut agent = Agent::new(
            AgentConfig {
                empty_response_retries: 2,
                ..AgentConfig::default()
            },
            Vec::new(),
        );
        let err = agent.prompt(&provider, "hi").await.expect_err("must fail");
        assert!(
            matches!(err, OneError::EmptyResponse { attempts: 3 }),
            "got {err:?}"
        );
        // 1 initial + 2 retries
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        // Must not leave a blank assistant message in history.
        assert!(
            !agent.messages.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant(a) if a.content.is_empty()
                    || completion_is_empty(&CompletionResponse {
                        provider: a.provider.clone(),
                        model: a.model.clone(),
                        content: a.content.clone(),
                        stop_reason: a.stop_reason,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
            )),
            "empty assistant should not be committed on failure"
        );
    }

    #[test]
    fn retryable_provider_errors_and_backoff_are_classified() {
        assert!(is_retryable_provider_error(&OneError::Provider(
            "The model is currently at capacity due to high demand".into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "upstream request failed (status 503)".into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "openai chat/completions 500 Internal Server Error: {\"error\": \"server error\"}"
                .into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "anthropic 529: site overloaded".into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "gemini 503: service unavailable".into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "ollama 502: bad gateway".into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "error decoding response body: connection closed before message completed".into()
        )));
        assert!(is_retryable_provider_error(&OneError::Provider(
            "error decoding response body".into()
        )));
        assert!(!is_retryable_provider_error(&OneError::Provider(
            "invalid API key".into()
        )));
        assert!(!is_retryable_provider_error(&OneError::Provider(
            "openai chat/completions 401 Unauthorized: invalid_api_key".into()
        )));
        assert_eq!(retry_backoff_delay(1), Duration::from_secs(2));
        assert_eq!(retry_backoff_delay(4), Duration::from_secs(8));
        assert_eq!(retry_backoff_delay(10), Duration::from_secs(20));
    }

    #[tokio::test]
    async fn temporary_provider_errors_retry_then_succeed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CapacityThenText {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for CapacityThenText {
            fn name(&self) -> &str {
                "capacity-then-text"
            }

            fn model(&self) -> &str {
                "test"
            }

            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call < 2 {
                    Err(OneError::Provider("model at capacity".into()))
                } else {
                    Ok(empty_resp(vec![ContentBlock::text("recovered")]))
                }
            }
        }

        let provider = CapacityThenText {
            calls: AtomicUsize::new(0),
        };
        let events = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
        let mut agent = Agent::new(
            AgentConfig {
                empty_response_retries: 2,
                ..AgentConfig::default()
            },
            Vec::new(),
        );
        let collector = events.clone();
        agent.subscribe(Box::new(move |event| {
            collector.lock().expect("events").push(event.clone());
        }));

        let out = agent.prompt(&provider, "hi").await.expect("should recover");
        assert_eq!(out, "recovered");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        let events = events.lock().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::RetryScheduled { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::RetryStarted { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn empty_response_retries_disabled_fails_immediately() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AlwaysEmpty {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for AlwaysEmpty {
            fn name(&self) -> &str {
                "always-empty"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(empty_resp(vec![]))
            }
        }

        let provider = AlwaysEmpty {
            calls: AtomicUsize::new(0),
        };
        let mut agent = Agent::new(
            AgentConfig {
                empty_response_retries: 0,
                ..AgentConfig::default()
            },
            Vec::new(),
        );
        let err = agent.prompt(&provider, "hi").await.expect_err("must fail");
        assert!(matches!(err, OneError::EmptyResponse { attempts: 1 }));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stop_hook_blocks_agent_turn_and_feeds_back_message() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MultiTurnProvider {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for MultiTurnProvider {
            fn name(&self) -> &str {
                "multi-turn"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(empty_resp(vec![ContentBlock::text("draft done")]))
                } else {
                    let has_feedback = req.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => match &u.content {
                            UserContent::Text(t) => t.contains("tests failed, please fix"),
                            UserContent::Blocks(blocks) => blocks.iter().any(|b| match b {
                                TextOrImage::Text { text } => {
                                    text.contains("tests failed, please fix")
                                }
                                _ => false,
                            }),
                        },
                        _ => false,
                    });
                    assert!(has_feedback, "Agent must receive stop hook feedback");
                    Ok(empty_resp(vec![ContentBlock::text("fixed and completed")]))
                }
            }
        }

        struct TestStopHooks {
            stop_calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl AgentHooks for TestStopHooks {
            async fn on_stop(
                &self,
                _turn: usize,
                _last_assistant_message: Option<&str>,
            ) -> StopDecision {
                let count = self.stop_calls.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    StopDecision::Block {
                        reason: "tests failed, please fix".into(),
                    }
                } else {
                    StopDecision::Allow
                }
            }
        }

        let provider = MultiTurnProvider {
            calls: AtomicUsize::new(0),
        };
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let stop_hooks = Arc::new(TestStopHooks {
            stop_calls: AtomicUsize::new(0),
        });
        agent.set_hooks(Some(stop_hooks.clone()));

        let result = agent
            .prompt(&provider, "do task")
            .await
            .expect("agent should succeed");
        assert_eq!(result, "fixed and completed");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(stop_hooks.stop_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn parallel_readonly_tools_respect_concurrency_cap_and_order() {
        use crate::tool::{Tool, ToolDefinition};
        use crate::tool_gate::{ToolGate, ToolGateDecision};
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        struct CountingRead {
            current: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            hold: Duration,
        }

        #[async_trait::async_trait]
        impl Tool for CountingRead {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "read".into(),
                    description: "slow read".into(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }
            async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
                let n = self.current.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(self.hold).await;
                self.current.fetch_sub(1, Ordering::SeqCst);
                Ok(ToolOutput::text(format!("done-{}", call.id)))
            }
        }

        struct AllowGate {
            released: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ToolGate for AllowGate {
            async fn check(&self, _call: &ToolCall) -> ToolGateDecision {
                ToolGateDecision::Allow
            }
            fn release_permission_lease(&self, _call: &ToolCall) {
                self.released.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct BatchProvider;

        #[async_trait::async_trait]
        impl LlmProvider for BatchProvider {
            fn name(&self) -> &str {
                "batch"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let has_tools = request
                    .messages
                    .iter()
                    .any(|m| matches!(m, AgentMessage::ToolResult(_)));
                if has_tools {
                    return Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::Text {
                            text: "done".into(),
                        }],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    });
                }
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: (0..4)
                        .map(|i| ContentBlock::ToolCall {
                            id: format!("c{i}"),
                            name: "read".into(),
                            arguments: serde_json::json!({}),
                        })
                        .collect(),
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            AgentConfig {
                max_parallel_readonly_tools: 2,
                ..AgentConfig::default()
            },
            vec![Arc::new(CountingRead {
                current,
                peak: peak.clone(),
                hold: Duration::from_millis(40),
            })],
        );
        agent.set_tool_gate(Some(Arc::new(AllowGate {
            released: released.clone(),
        })));
        let out = agent.prompt(&BatchProvider, "go").await.expect("prompt");
        assert_eq!(out, "done");
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak={}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(released.load(Ordering::SeqCst), 4);
        let results: Vec<_> = agent
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(tr) => {
                    Some((tr.tool_call_id.as_str(), tr.content.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 4);
        for (i, (id, _)) in results.iter().enumerate() {
            assert_eq!(*id, format!("c{i}"));
        }
    }

    struct MockTaskWaiter {
        terminals: std::sync::Mutex<std::collections::HashSet<String>>,
        notifiers: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>,
    }

    impl MockTaskWaiter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                terminals: std::sync::Mutex::new(std::collections::HashSet::new()),
                notifiers: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        }
        fn get_or_create_notify(&self, id: &str) -> Arc<tokio::sync::Notify> {
            let mut map = self.notifiers.lock().unwrap();
            map.entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
                .clone()
        }
        fn complete(&self, id: &str) {
            let notify = {
                let mut terms = self.terminals.lock().unwrap();
                terms.insert(id.to_string());
                self.get_or_create_notify(id)
            };
            notify.notify_waiters();
        }
    }

    impl TaskWaitWaiter for MockTaskWaiter {
        fn is_terminal(&self, id: &str) -> bool {
            self.terminals.lock().unwrap().contains(id)
        }
        fn subscribe_terminal<'a>(
            &'a self,
            id: &'a str,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            let notify = self.get_or_create_notify(id);
            Box::pin(RegisteredWaiter::new(notify))
        }
    }

    #[tokio::test]
    async fn react_park_and_wake_on_task_completion() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct TurnCountingProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for TurnCountingProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    // Turn 0: LLM finishes its tool calls and has nothing more to do, waiting on background
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting for task...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    // Turn 1: LLM sees the drained completion notification
                    let has_notice = request.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => {
                            u.content.as_plain_text().contains("bg_1 completed")
                        }
                        _ => false,
                    });
                    assert!(
                        has_notice,
                        "drained notification must be present in LLM turn"
                    );
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("all finished!")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_1".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let waiter_clone = waiter.clone();
        let agent_notification_queue = agent.notification_queue_handle();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            agent_notification_queue
                .lock()
                .unwrap()
                .push("bg_1 completed".into());
            waiter_clone.complete("bg_1");
        });

        let provider = TurnCountingProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "all finished!");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn react_park_respects_any_mode() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AnyProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for AnyProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting for any...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("one done!")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_1".into(), "bg_2".into()],
            WaitInterestMode::Any,
            waiter.clone(),
        )));

        let waiter_clone = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            waiter_clone.complete("bg_2");
        });

        let provider = AnyProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "one done!");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
        assert!(!waiter.is_terminal("bg_1"));
        assert!(waiter.is_terminal("bg_2"));
    }

    #[tokio::test]
    async fn react_park_respects_all_mode() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AllProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for AllProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting for all...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("both done!")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_1".into(), "bg_2".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let waiter_clone = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            waiter_clone.complete("bg_1");
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            waiter_clone.complete("bg_2");
        });

        let provider = AllProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "both done!");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn react_unrelated_notification_does_not_wake() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct UnrelatedProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for UnrelatedProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("target finished")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["target".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let waiter_clone = waiter.clone();
        let q = agent.notification_queue_handle();
        tokio::spawn(async move {
            // Push unrelated notification (e.g. monitor line or dev server output)
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            q.lock()
                .unwrap()
                .push("unrelated monitor output line".into());
            waiter_clone.complete("unrelated_task");

            // Later, complete target
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            waiter_clone.complete("target");
        });

        let provider = UnrelatedProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "target finished");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn react_unawaited_background_task_does_not_park() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SimpleProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for SimpleProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                self.turns.fetch_add(1, Ordering::SeqCst);
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: vec![ContentBlock::text("immediate completion")],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        // An unawaited background task running (no WaitInterest registered)
        let agent = Agent::new(AgentConfig::default(), Vec::new());
        // wait_interest is None
        let mut agent = agent;
        let provider = SimpleProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "immediate completion");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn react_instant_completion_no_lost_wakeup() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct InstantProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for InstantProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("done")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        // Task is already complete BEFORE agent runs / checks
        waiter.complete("bg_instant");

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_instant".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let provider = InstantProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "waiting..."); // Because interest was satisfied immediately, ReAct didn't park
    }

    #[tokio::test]
    async fn react_abort_interrupts_park() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct BlockedProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for BlockedProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                self.turns.fetch_add(1, Ordering::SeqCst);
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: vec![ContentBlock::text("waiting forever...")],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_forever".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let abort_flag = agent.abort_handle();
        let input_waker = agent.input_waker_handle();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            abort_flag.store(true, Ordering::Relaxed);
            input_waker.notify_one();
        });

        let provider = BlockedProvider {
            turns: AtomicUsize::new(0),
        };
        let err = agent.prompt(&provider, "start").await;
        assert!(err.is_err(), "aborted run should return error/aborted");
    }

    #[tokio::test]
    async fn react_steering_input_wakes_park() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SteerProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for SteerProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    let has_steer = request.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => {
                            u.content.as_plain_text().contains("steer message")
                        }
                        _ => false,
                    });
                    assert!(has_steer, "steering message must be delivered to LLM turn");
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("handled steering")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_long".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let steering_queue = agent.steering_queue_handle();
        let input_waker = agent.input_waker_handle();
        let waiter_clone = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            steering_queue.lock().unwrap().push("steer message".into());
            input_waker.notify_one();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            waiter_clone.complete("bg_long");
        });

        let provider = SteerProvider {
            turns: AtomicUsize::new(0),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "handled steering");
        // Exactly 2 turns: steer wakes the park, the next stop ends the run.
        // The wait interest was cleared on NewInput — no re-park after the
        // steer is consumed (stale waiting state would hang until bg completes).
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn steer_message_keeps_steer_kind_and_serializes() {
        let msg = AgentMessage::steer_text("不要重构 shared package");
        let AgentMessage::User(u) = &msg else {
            panic!("steer must stay role=user for providers");
        };
        assert!(u.is_steer());
        assert_eq!(u.kind.as_deref(), Some("steer"));
        // Round-trip through the session/jsonl wire format.
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(
            json.contains("\"kind\":\"steer\""),
            "wire carries kind: {json}"
        );
        let back: AgentMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, msg);

        // Plain user turns stay kind-free (old format unchanged).
        let plain = AgentMessage::user_text("real turn");
        let AgentMessage::User(pu) = &plain else {
            panic!()
        };
        assert!(!pu.is_steer());
        let plain_json = serde_json::to_string(&plain).expect("serialize");
        assert!(!plain_json.contains("kind"), "old format stays minimal");
        // Legacy sessions without `kind` still deserialize as user turns.
        let legacy = r#"{"role":"user","content":"old prompt","timestamp":123}"#;
        let m: AgentMessage = serde_json::from_str(legacy).expect("legacy parse");
        assert!(matches!(&m, AgentMessage::User(u) if !u.is_steer()));
    }

    #[tokio::test]
    async fn steer_drain_tags_message_and_emits_event() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Mutex};

        // Provider: first turn parks on a background waiter; steering wakes it.
        struct P {
            turns: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl LlmProvider for P {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn > 0 {
                    return Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("done")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    });
                }
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: vec![ContentBlock::text("waiting")],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_job".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_sink = seen.clone();
        agent.subscribe(Box::new(move |event: &AgentEvent| {
            if let AgentEvent::SteerApplied { text } = event {
                seen_sink.lock().unwrap().push(text.clone());
            }
        }));

        let steering_queue = agent.steering_queue_handle();
        let input_waker = agent.input_waker_handle();
        let waiter_clone = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            steering_queue.lock().unwrap().push("steer now".into());
            input_waker.notify_one();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            waiter_clone.complete("bg_job");
        });

        let provider = P {
            turns: AtomicUsize::new(0),
        };
        agent.prompt(&provider, "start").await.expect("run");

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &["steer now".to_string()],
            "SteerApplied must fire once with the steer text"
        );
        let steers = agent
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User(u) if u.is_steer()))
            .count();
        assert_eq!(steers, 1, "drained steer is tagged kind=steer in context");
        let turns = crate::compaction::user_turn_count(&agent.messages);
        assert_eq!(turns, 1, "steer must not count as a user turn boundary");
    }

    // ---- Acceptance: real park (blocked in tokio::select) + external wake ----

    /// Shared provider for park acceptance tests: turn 0 returns text (parks via
    /// pre-set wait interest on the no-tool-call path); the final turn asserts
    /// the woken request contains `expect_snippet` and returns "done".
    struct ParkProvider {
        turns: std::sync::atomic::AtomicUsize,
        expect_snippet: Option<&'static str>,
    }
    #[async_trait]
    impl LlmProvider for ParkProvider {
        fn name(&self) -> &str {
            "park-provider"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            let turn = self.turns.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                return Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: vec![ContentBlock::text("parking")],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                });
            }
            if let Some(snippet) = self.expect_snippet {
                let hit = request.messages.iter().any(|m| match m {
                    AgentMessage::User(u) => u.content.as_plain_text().contains(snippet),
                    _ => false,
                });
                assert!(hit, "woken turn must see {snippet:?} in context");
            }
            Ok(CompletionResponse {
                provider: self.name().into(),
                model: self.model().into(),
                content: vec![ContentBlock::text("done")],
                stop_reason: StopReason::Stop,
                usage: TokenUsage::default(),
                citations: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn park_acceptance_steering_wakes_and_is_consumed() {
        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_a".into(), "bg_b".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        // After the agent is truly parked, inject a steer externally.
        let steering_queue = agent.steering_queue_handle();
        let input_waker = agent.input_waker_handle();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            Agent::push_queue(&steering_queue, "redirect to job_output");
            input_waker.notify_one();
        });

        let provider = ParkProvider {
            turns: std::sync::atomic::AtomicUsize::new(0),
            expect_snippet: Some("redirect to job_output"),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "done");
        assert_eq!(provider.turns.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Steer actually entered the model context as a tagged steer message.
        let steers = agent
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User(u) if u.is_steer()))
            .count();
        assert_eq!(steers, 1);
    }

    #[tokio::test]
    async fn park_acceptance_followup_wakes_and_enters_next_turn() {
        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_followup".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let followup_queue = agent.followup_queue_handle();
        let input_waker = agent.input_waker_handle();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            Agent::push_queue(&followup_queue, "followup: check results");
            input_waker.notify_one();
        });

        let provider = ParkProvider {
            turns: std::sync::atomic::AtomicUsize::new(0),
            expect_snippet: Some("followup: check results"),
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "done");
        assert_eq!(provider.turns.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn park_acceptance_abort_exits_immediately_without_extra_turns() {
        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_never_finishes".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        let abort_flag = agent.abort_handle();
        let input_waker = agent.input_waker_handle();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            abort_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            input_waker.notify_one();
        });

        let provider = ParkProvider {
            turns: std::sync::atomic::AtomicUsize::new(0),
            expect_snippet: None,
        };
        let started = std::time::Instant::now();
        let err = agent.prompt(&provider, "start").await;
        assert!(err.is_err(), "abort during park must end the run");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "abort must wake immediately, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            provider.turns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no further LLM turn after abort"
        );
        // Waiting state must not linger.
        assert!(
            agent.wait_interest_handle().lock().unwrap().is_none(),
            "interest cleared after abort"
        );
    }

    #[tokio::test]
    async fn park_acceptance_completion_wakes_and_clears_waiting_state() {
        use std::sync::{Arc, Mutex};

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_c1".into(), "bg_c2".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        // Track WaitParkStart/End pairing.
        let starts: Arc<Mutex<Vec<(WaitInterestMode, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let ends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let starts_cb = starts.clone();
        let ends_cb = ends.clone();
        agent.subscribe(Box::new(move |event| match event {
            AgentEvent::WaitParkStart { mode, ids } => {
                starts_cb.lock().unwrap().push((*mode, ids.len()));
            }
            AgentEvent::WaitParkEnd => {
                ends_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            _ => {}
        }));

        // Complete the tasks externally while the agent is parked.
        let waiter_clone = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            waiter_clone.complete("bg_c1");
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            waiter_clone.complete("bg_c2");
        });

        let provider = ParkProvider {
            turns: std::sync::atomic::AtomicUsize::new(0),
            expect_snippet: None,
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "done");
        assert_eq!(provider.turns.load(std::sync::atomic::Ordering::SeqCst), 2);

        let starts = starts.lock().unwrap().clone();
        assert_eq!(
            starts,
            vec![(WaitInterestMode::All, 2)],
            "one WaitParkStart with mode + id count"
        );
        assert_eq!(
            ends.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one WaitParkEnd — waiting state fully cleared"
        );
    }

    #[tokio::test]
    async fn park_acceptance_any_mode_wakes_on_first_completion() {
        use std::sync::{Arc, Mutex};

        let waiter = MockTaskWaiter::new();
        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_any1".into(), "bg_any2".into()],
            WaitInterestMode::Any,
            waiter.clone(),
        )));

        let starts: Arc<Mutex<Vec<(WaitInterestMode, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let starts_cb = starts.clone();
        agent.subscribe(Box::new(move |event| {
            if let AgentEvent::WaitParkStart { mode, ids } = event {
                starts_cb.lock().unwrap().push((*mode, ids.len()));
            }
        }));

        let waiter_clone = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            // Only the first target completes; the second never does.
            waiter_clone.complete("bg_any1");
        });

        let provider = ParkProvider {
            turns: std::sync::atomic::AtomicUsize::new(0),
            expect_snippet: None,
        };
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "done");
        assert_eq!(provider.turns.load(std::sync::atomic::Ordering::SeqCst), 2);

        let starts = starts.lock().unwrap().clone();
        assert_eq!(
            starts,
            vec![(WaitInterestMode::Any, 2)],
            "any-mode interest carries mode + both ids"
        );
        assert!(waiter.is_terminal("bg_any1"));
        assert!(!waiter.is_terminal("bg_any2"));
    }

    struct DeterministicRaceWaiter<F: Fn() + Send + Sync + 'static> {
        base: Arc<MockTaskWaiter>,
        hook: F,
        called: AtomicBool,
    }

    impl<F: Fn() + Send + Sync + 'static> DeterministicRaceWaiter<F> {
        fn new(hook: F) -> Self {
            Self {
                base: MockTaskWaiter::new(),
                hook,
                called: AtomicBool::new(false),
            }
        }
        fn complete(&self, id: &str) {
            self.base.complete(id);
        }
    }

    impl<F: Fn() + Send + Sync + 'static> TaskWaitWaiter for DeterministicRaceWaiter<F> {
        fn is_terminal(&self, id: &str) -> bool {
            self.base.is_terminal(id)
        }
        fn subscribe_terminal<'a>(
            &'a self,
            id: &'a str,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            if !self.called.swap(true, Ordering::SeqCst) {
                (self.hook)();
            }
            self.base.subscribe_terminal(id)
        }
    }

    #[tokio::test]
    async fn react_deterministic_race_steering_during_subscribe_does_not_lose_wakeup() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let steering_queue = agent.steering_queue_handle();
        let input_waker = agent.input_waker_handle();

        // Hook injects steering synchronously right when subscribe_terminal is called
        let waiter = Arc::new(DeterministicRaceWaiter::new(move || {
            steering_queue
                .lock()
                .unwrap()
                .push("race steering message".into());
            input_waker.notify_one();
        }));

        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_never".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        // Use a generic waiter type by storing an Arc<MockTaskWaiter> directly
        struct SteerProvider2<W: TaskWaitWaiter + 'static> {
            turns: AtomicUsize,
            waiter: Arc<W>,
            complete_fn: Box<dyn Fn(&W) + Send + Sync>,
        }

        #[async_trait::async_trait]
        impl<W: TaskWaitWaiter + 'static> LlmProvider for SteerProvider2<W> {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else if turn == 1 {
                    let has_steer = request.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => {
                            u.content.as_plain_text().contains("race steering message")
                        }
                        _ => false,
                    });
                    assert!(has_steer, "steering message must be delivered to LLM turn");
                    (self.complete_fn)(&self.waiter);
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("handled race steering")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("final")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let provider = SteerProvider2 {
            turns: AtomicUsize::new(0),
            waiter: waiter.clone(),
            complete_fn: Box::new(|w| w.complete("bg_never")),
        };
        // The background task NEVER finishes on its own; the raced steering message must wake ReAct immediately!
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "handled race steering");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn react_deterministic_race_followup_during_subscribe_does_not_lose_wakeup() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let followup_queue = agent.followup_queue_handle();
        let input_waker = agent.input_waker_handle();

        // Hook injects followup synchronously right when subscribe_terminal is called
        let waiter = Arc::new(DeterministicRaceWaiter::new(move || {
            followup_queue
                .lock()
                .unwrap()
                .push("race followup message".into());
            input_waker.notify_one();
        }));

        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_never".into()],
            WaitInterestMode::All,
            waiter.clone(),
        )));

        struct FollowupProvider2<W: TaskWaitWaiter + 'static> {
            turns: AtomicUsize,
            waiter: Arc<W>,
            complete_fn: Box<dyn Fn(&W) + Send + Sync>,
        }

        #[async_trait::async_trait]
        impl<W: TaskWaitWaiter + 'static> LlmProvider for FollowupProvider2<W> {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.turns.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("waiting...")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else if turn == 1 {
                    let has_followup = request.messages.iter().any(|m| match m {
                        AgentMessage::User(u) => {
                            u.content.as_plain_text().contains("race followup message")
                        }
                        _ => false,
                    });
                    assert!(
                        has_followup,
                        "followup message must be delivered to LLM turn"
                    );
                    (self.complete_fn)(&self.waiter);
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("handled race followup")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("final")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                }
            }
        }

        let provider = FollowupProvider2 {
            turns: AtomicUsize::new(0),
            waiter: waiter.clone(),
            complete_fn: Box::new(|w| w.complete("bg_never")),
        };
        // The background task NEVER finishes on its own; the raced followup message must wake ReAct immediately!
        let out = agent.prompt(&provider, "start").await.expect("run");
        assert_eq!(out, "handled race followup");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn react_deterministic_race_abort_during_subscribe_does_not_lose_wakeup() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AbortProvider {
            turns: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl LlmProvider for AbortProvider {
            fn name(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                self.turns.fetch_add(1, Ordering::SeqCst);
                Ok(CompletionResponse {
                    provider: self.name().into(),
                    model: self.model().into(),
                    content: vec![ContentBlock::text("waiting...")],
                    stop_reason: StopReason::Stop,
                    usage: TokenUsage::default(),
                    citations: Vec::new(),
                })
            }
        }

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let abort_flag = agent.abort_handle();
        let input_waker = agent.input_waker_handle();

        // Hook injects abort synchronously right when subscribe_terminal is called
        let waiter = Arc::new(DeterministicRaceWaiter::new(move || {
            abort_flag.store(true, Ordering::Relaxed);
            input_waker.notify_one();
        }));

        agent.set_wait_interest(Some(WaitInterest::new(
            vec!["bg_never".into()],
            WaitInterestMode::All,
            waiter,
        )));

        let provider = AbortProvider {
            turns: AtomicUsize::new(0),
        };
        // The background task NEVER finishes, but the raced abort must terminate park immediately!
        let err = agent.prompt(&provider, "start").await;
        assert!(err.is_err(), "aborted run should return error");
        assert_eq!(provider.turns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn react_park_deterministic_lifecycle_and_single_agent_end() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::sync::Notify;

        #[derive(Default)]
        struct EventTracker {
            agent_starts: AtomicUsize,
            agent_ends: AtomicUsize,
            agent_end_seen_before_task_terminal: AtomicBool,
        }

        let tracker = Arc::new(EventTracker::default());

        let tool_executed = Arc::new(Notify::new());
        let task_terminal = Arc::new(AtomicBool::new(false));
        let second_complete_entered = Arc::new(AtomicBool::new(false));
        let provider_invocations = Arc::new(AtomicUsize::new(0));

        struct MockTool {
            tool_executed: Arc<Notify>,
            wait_interest_handle: Arc<std::sync::Mutex<Option<WaitInterest>>>,
            waiter: Arc<MockTaskWaiter>,
        }

        #[async_trait::async_trait]
        impl Tool for MockTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "test_wait_tasks".into(),
                    description: "mock wait tasks".into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                }
            }
            async fn execute(&self, _call: &ToolCall) -> Result<ToolOutput> {
                // Register active wait interest on "bg_task_1"
                *self.wait_interest_handle.lock().unwrap() = Some(WaitInterest::new(
                    vec!["bg_task_1".into()],
                    WaitInterestMode::All,
                    self.waiter.clone(),
                ));
                self.tool_executed.notify_waiters();
                Ok(ToolOutput::text("registered wait interest"))
            }
        }

        struct LifecycleProvider {
            invocations: Arc<AtomicUsize>,
            task_terminal: Arc<AtomicBool>,
            second_complete_entered: Arc<AtomicBool>,
            unblock_second_turn: Arc<Notify>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for LifecycleProvider {
            fn name(&self) -> &str {
                "lifecycle-provider"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
                let turn = self.invocations.fetch_add(1, Ordering::SeqCst);
                if turn == 0 {
                    // Turn 0: emit tool call to test_wait_tasks
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::ToolCall {
                            id: "tc_1".into(),
                            name: "test_wait_tasks".into(),
                            arguments: serde_json::json!({}),
                        }],
                        stop_reason: StopReason::ToolUse,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else if turn == 1 {
                    // Turn 1: MUST ONLY happen AFTER task is terminal!
                    assert!(
                        self.task_terminal.load(Ordering::SeqCst),
                        "provider complete must NOT be called for second turn before task is terminal!"
                    );
                    self.second_complete_entered.store(true, Ordering::SeqCst);
                    self.unblock_second_turn.notified().await;
                    Ok(CompletionResponse {
                        provider: self.name().into(),
                        model: self.model().into(),
                        content: vec![ContentBlock::text("final answer")],
                        stop_reason: StopReason::Stop,
                        usage: TokenUsage::default(),
                        citations: Vec::new(),
                    })
                } else {
                    panic!("unexpected third completion call");
                }
            }
        }

        let waiter = MockTaskWaiter::new();
        let unblock_second_turn = Arc::new(Notify::new());

        let mut agent = Agent::new(AgentConfig::default(), Vec::new());
        let wait_interest_handle = agent.wait_interest_handle();

        let tool = Arc::new(MockTool {
            tool_executed: tool_executed.clone(),
            wait_interest_handle,
            waiter: waiter.clone(),
        });
        agent.set_tools(vec![tool]);

        let tracker_for_cb = tracker.clone();
        let task_terminal_for_cb = task_terminal.clone();
        agent.subscribe(Box::new(move |event| match event {
            AgentEvent::AgentStart => {
                tracker_for_cb.agent_starts.fetch_add(1, Ordering::SeqCst);
            }
            AgentEvent::AgentEnd { .. } => {
                tracker_for_cb.agent_ends.fetch_add(1, Ordering::SeqCst);
                if !task_terminal_for_cb.load(Ordering::SeqCst) {
                    tracker_for_cb
                        .agent_end_seen_before_task_terminal
                        .store(true, Ordering::SeqCst);
                }
            }
            _ => {}
        }));

        let provider = LifecycleProvider {
            invocations: provider_invocations.clone(),
            task_terminal: task_terminal.clone(),
            second_complete_entered: second_complete_entered.clone(),
            unblock_second_turn: unblock_second_turn.clone(),
        };

        let notification_handle = agent.notification_queue_handle();
        let agent_task =
            tokio::spawn(async move { agent.prompt(&provider, "do task and wait").await });

        // 1. Wait until tool executes and registers WaitInterest
        tool_executed.notified().await;

        // Yield slightly so ReAct finishes tool batch, sees unsatisfied WaitInterest, and parks
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // 2. Verify deterministic state during park:
        // - AgentStart has occurred
        assert_eq!(tracker.agent_starts.load(Ordering::SeqCst), 1);
        // - AgentEnd has NOT occurred
        assert_eq!(tracker.agent_ends.load(Ordering::SeqCst), 0);
        assert!(
            !tracker
                .agent_end_seen_before_task_terminal
                .load(Ordering::SeqCst),
            "AgentEnd must not fire during park"
        );
        // - Provider has NOT received second complete call
        assert_eq!(provider_invocations.load(Ordering::SeqCst), 1);
        assert!(!second_complete_entered.load(Ordering::SeqCst));

        // 3. Complete the background task (task terminal)
        task_terminal.store(true, Ordering::SeqCst);
        notification_handle
            .lock()
            .unwrap()
            .push("bg_task_1 completed".into());
        waiter.complete("bg_task_1");

        // 4. Wait for provider to enter second complete call (woken up)
        while !second_complete_entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Provider is now in second complete call; AgentEnd must still NOT have fired!
        assert_eq!(tracker.agent_ends.load(Ordering::SeqCst), 0);
        assert_eq!(provider_invocations.load(Ordering::SeqCst), 2);

        // 5. Unblock second turn completion so agent run can finish
        unblock_second_turn.notify_one();

        let out = agent_task.await.expect("join").expect("prompt success");
        assert_eq!(out, "final answer");

        // 6. Verify final lifecycle:
        // - Exactly one AgentStart and exactly one AgentEnd
        assert_eq!(tracker.agent_starts.load(Ordering::SeqCst), 1);
        assert_eq!(tracker.agent_ends.load(Ordering::SeqCst), 1);
        assert!(!tracker
            .agent_end_seen_before_task_terminal
            .load(Ordering::SeqCst));
    }
}
