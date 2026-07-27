//! Optional execution-trace recording for harness eval and comparison.
//!
//! Design goals:
//! - **Additive only**: agents work without a sink (zero cost when unset).
//! - **Core stays pure**: sinks may be in-memory or JSONL; disk paths are caller's concern.
//! - **Stable schema**: tagged JSON (`type` field) suitable for cross-agent normalize.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::TokenUsage;
use crate::message::{now_ms, AgentMessage, UserContent};

/// Default preview size for tool args / short run output (chars).
pub const PREVIEW_DEFAULT_CHARS: usize = 240;
/// Larger preview when `--trace-full` is set (chars).
pub const PREVIEW_FULL_CHARS: usize = 16_384;
/// Default budget for generation observation input/output (full messages).
/// Large enough for multi-turn context; individual tool outputs are truncated first.
pub const PREVIEW_LLM_CHARS: usize = 16_384;
/// Per-tool-result content cap inside generation input (chars), before total budget.
pub const PREVIEW_TOOL_RESULT_CHARS: usize = 2_048;
/// System prompt cap inside generation input (chars).
pub const PREVIEW_SYSTEM_CHARS: usize = 4_096;

/// Where traces go. Implementations must be cheap and non-panicking.
pub trait TraceSink: Send + Sync {
    fn record(&self, event: TraceEvent);
}

/// No-op sink (explicit placeholder).
pub struct NullTrace;

impl TraceSink for NullTrace {
    fn record(&self, _event: TraceEvent) {}
}

/// In-memory sink for tests and post-run analysis.
#[derive(Default)]
pub struct MemoryTrace {
    events: Mutex<Vec<TraceEvent>>,
}

impl MemoryTrace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<TraceEvent> {
        self.events.lock().expect("trace lock").clone()
    }

    pub fn clear(&self) {
        self.events.lock().expect("trace lock").clear();
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("trace lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TraceSink for MemoryTrace {
    fn record(&self, event: TraceEvent) {
        self.events.lock().expect("trace lock").push(event);
    }
}

/// Append-only JSONL file sink (one event per line).
pub struct JsonlTraceSink {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}

impl JsonlTraceSink {
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = File::create(&path)?;
        Ok(Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn flush(&self) -> std::io::Result<()> {
        self.writer.lock().expect("trace writer").flush()
    }
}

impl TraceSink for JsonlTraceSink {
    fn record(&self, event: TraceEvent) {
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        if let Ok(mut w) = self.writer.lock() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
}

/// Shared sink handle used by [`crate::agent::Agent`].
pub type SharedTrace = Arc<dyn TraceSink>;

/// Gate outcome for tooling / permission friction analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceGateDecision {
    Allow,
    Rewrite,
    Deny,
}

impl TraceGateDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Rewrite => "rewrite",
            Self::Deny => "deny",
        }
    }
}

/// Run outcome for the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceRunStatus {
    Ok,
    Aborted,
    MaxTurns,
    Error,
}

/// One structured span / event in an agent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEvent {
    RunStart {
        ts_ms: u64,
        run_id: String,
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_id: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        trace_full: bool,
        /// User-facing input for the root agent observation / trace list preview.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_preview: Option<String>,
    },
    RunEnd {
        ts_ms: u64,
        run_id: String,
        status: TraceRunStatus,
        turns: usize,
        wall_ms: u64,
        #[serde(default, skip_serializing_if = "TokenUsage::is_zero")]
        usage: TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_text_len: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_text_preview: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    TurnStart {
        ts_ms: u64,
        run_id: String,
        turn: usize,
        message_count: usize,
        tools_n: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_prompt_tokens: Option<u64>,
    },
    LlmRequest {
        ts_ms: u64,
        run_id: String,
        turn: usize,
        message_count: usize,
        tools_n: usize,
        system_prompt_len: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_preview: Option<String>,
    },
    LlmResponse {
        ts_ms: u64,
        run_id: String,
        turn: usize,
        latency_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        ttft_ms: Option<u64>,
        stop_reason: String,
        tool_calls_n: usize,
        text_len: usize,
        thinking_len: usize,
        #[serde(default, skip_serializing_if = "TokenUsage::is_zero")]
        usage: TokenUsage,
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
        /// Structured tool calls requested by this generation (for Langfuse UI).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<TraceToolCall>,
    },
    ToolStart {
        ts_ms: u64,
        run_id: String,
        turn: usize,
        call_id: String,
        name: String,
        args_bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        args_preview: Option<String>,
    },
    ToolEnd {
        ts_ms: u64,
        run_id: String,
        turn: usize,
        call_id: String,
        name: String,
        duration_ms: u64,
        is_error: bool,
        output_bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        gate: Option<TraceGateDecision>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
    },
    Gate {
        ts_ms: u64,
        run_id: String,
        turn: usize,
        call_id: String,
        name: String,
        decision: TraceGateDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Compaction {
        ts_ms: u64,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        before_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Optional external scorer result (written by bench, not by the agent loop).
    Score {
        ts_ms: u64,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        pass: bool,
        score: f64,
        checks: Vec<ScoreCheckResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
    },
}

/// One tool call attached to a generation observation (id / name / args).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceToolCall {
    pub id: String,
    pub name: String,
    /// Arguments JSON, already size-bounded for tracing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreCheckResult {
    pub name: String,
    pub pass: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Build structured tool-call snapshots for generation traces.
pub fn trace_tool_calls(
    calls: &[crate::tool::ToolCall],
    max_arg_chars: usize,
) -> Vec<TraceToolCall> {
    calls
        .iter()
        .map(|c| {
            let arguments = if max_arg_chars == 0 {
                None
            } else {
                let raw = serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".into());
                if raw.chars().count() <= max_arg_chars {
                    Some(c.arguments.clone())
                } else {
                    // Keep a truncated string form so we don't ship huge args.
                    text_preview(&raw, max_arg_chars).map(Value::String)
                }
            };
            TraceToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments,
            }
        })
        .collect()
}

impl TraceEvent {
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::RunStart { run_id, .. }
            | Self::RunEnd { run_id, .. }
            | Self::TurnStart { run_id, .. }
            | Self::LlmRequest { run_id, .. }
            | Self::LlmResponse { run_id, .. }
            | Self::ToolStart { run_id, .. }
            | Self::ToolEnd { run_id, .. }
            | Self::Gate { run_id, .. }
            | Self::Compaction { run_id, .. }
            | Self::Score { run_id, .. } => Some(run_id.as_str()),
        }
    }
}

/// Preview of tool args for traces (bounded size, not a security boundary).
pub fn args_preview(args: &Value, max_chars: usize) -> (usize, Option<String>) {
    let raw = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
    let bytes = raw.len();
    (bytes, text_preview(&raw, max_chars))
}

/// Bound a string for trace / Langfuse observation input-output fields.
pub fn text_preview(s: &str, max_chars: usize) -> Option<String> {
    if max_chars == 0 || s.is_empty() {
        return None;
    }
    if s.chars().count() <= max_chars {
        Some(s.to_string())
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        Some(format!("{truncated}…"))
    }
}

/// Generation observation output: structured assistant message JSON.
///
/// Shape:
/// ```json
/// {"role":"assistant","content":"...","tool_calls":[{"id":"...","name":"...","arguments":{}}]}
/// ```
pub fn llm_output_preview(
    text: &str,
    tool_calls: &[crate::tool::ToolCall],
    max_chars: usize,
) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    let content = if text.is_empty() {
        Value::Null
    } else {
        // Leave room for tool_calls + envelope; content is the main truncation target.
        let content_budget = max_chars.saturating_sub(128).max(64);
        Value::String(text_preview(text, content_budget).unwrap_or_default())
    };
    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), Value::String("assistant".into()));
    msg.insert("content".into(), content);
    if !tool_calls.is_empty() {
        // Cap each arg blob so huge tool args don't blow the observation.
        let arg_budget = (max_chars / tool_calls.len().max(1)).clamp(256, 4_096);
        let tcs: Vec<Value> = tool_calls
            .iter()
            .map(|c| {
                let arguments = {
                    let raw = serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".into());
                    if raw.chars().count() <= arg_budget {
                        c.arguments.clone()
                    } else {
                        text_preview(&raw, arg_budget)
                            .map(Value::String)
                            .unwrap_or(Value::String("…".into()))
                    }
                };
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "arguments": arguments,
                })
            })
            .collect();
        msg.insert("tool_calls".into(), Value::Array(tcs));
    }
    let raw = serde_json::to_string(&Value::Object(msg)).ok()?;
    text_preview(&raw, max_chars)
}

/// Short root-agent / list preview: last user message text only.
pub fn last_user_preview(messages: &[AgentMessage], max_chars: usize) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    for m in messages.iter().rev() {
        if let AgentMessage::User(u) = m {
            let text = match &u.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        crate::message::TextOrImage::Text { text } => text.as_str(),
                        crate::message::TextOrImage::Image { .. } => "[image]",
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            return text_preview(&text, max_chars);
        }
    }
    None
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                crate::message::TextOrImage::Text { text } => text.as_str(),
                crate::message::TextOrImage::Image { .. } => "[image]",
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn text_or_image_join(blocks: &[crate::message::TextOrImage]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            crate::message::TextOrImage::Text { text } => text.as_str(),
            crate::message::TextOrImage::Image { .. } => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serialize one conversation message into an OpenAI-style role object for traces.
fn message_to_trace_value(
    m: &AgentMessage,
    tool_result_max: usize,
    arg_max: usize,
) -> Value {
    match m {
        AgentMessage::User(u) => {
            serde_json::json!({
                "role": "user",
                "content": user_content_text(&u.content),
            })
        }
        AgentMessage::Assistant(a) => {
            let mut content_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &a.content {
                match b {
                    crate::message::ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            content_parts.push(text.clone());
                        }
                    }
                    crate::message::ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        let raw =
                            serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into());
                        let args = if raw.chars().count() <= arg_max {
                            arguments.clone()
                        } else {
                            text_preview(&raw, arg_max)
                                .map(Value::String)
                                .unwrap_or(Value::String("…".into()))
                        };
                        tool_calls.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "arguments": args,
                        }));
                    }
                    crate::message::ContentBlock::Thinking { .. } => {
                        // Omit thinking body from generation input (size + privacy).
                    }
                }
            }
            let content = if content_parts.is_empty() {
                Value::Null
            } else {
                Value::String(content_parts.join("\n"))
            };
            let mut obj = serde_json::Map::new();
            obj.insert("role".into(), Value::String("assistant".into()));
            obj.insert("content".into(), content);
            if !tool_calls.is_empty() {
                obj.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            Value::Object(obj)
        }
        AgentMessage::ToolResult(t) => {
            let raw = text_or_image_join(&t.content);
            let content = text_preview(&raw, tool_result_max).unwrap_or_default();
            serde_json::json!({
                "role": "tool",
                "tool_call_id": t.tool_call_id,
                "name": t.tool_name,
                "content": content,
                "is_error": t.is_error,
            })
        }
    }
}

/// Generation input: actual messages sent to the model (OpenAI-style array).
///
/// Always includes system + full conversation when budget allows. Large tool
/// results are truncated first; if still over budget, oldest non-system
/// messages are dropped from the front (keeping the latest context).
///
/// Example shape:
/// ```json
/// [
///   {"role":"system","content":"..."},
///   {"role":"user","content":"这个项目干啥的"},
///   {"role":"assistant","content":null,"tool_calls":[{"id":"…","name":"ls","arguments":{}}]},
///   {"role":"tool","tool_call_id":"…","name":"ls","content":"…"}
/// ]
/// ```
pub fn llm_input_preview(
    system_prompt: &str,
    messages: &[AgentMessage],
    max_chars: usize,
) -> Option<String> {
    if max_chars == 0 {
        return None;
    }

    let system_cap = PREVIEW_SYSTEM_CHARS.min(max_chars.saturating_sub(64).max(64));
    let tool_result_cap = PREVIEW_TOOL_RESULT_CHARS.min(max_chars / 4).max(256);
    let arg_cap = 1_024usize.min(max_chars / 8).max(128);

    let system_content = text_preview(system_prompt, system_cap).unwrap_or_default();
    let system_msg = serde_json::json!({
        "role": "system",
        "content": system_content,
    });

    // Build all messages, then drop oldest until under budget (keep system + tail).
    let mut body: Vec<Value> = messages
        .iter()
        .map(|m| message_to_trace_value(m, tool_result_cap, arg_cap))
        .collect();

    loop {
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(system_msg.clone());
        payload.extend(body.iter().cloned());
        let raw = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(_) => return None,
        };
        if raw.chars().count() <= max_chars {
            return Some(raw);
        }
        // Prefer dropping oldest conversation turns over aggressive global truncate.
        if body.len() > 1 {
            body.remove(0);
            continue;
        }
        // Single remaining message (or empty): hard-cap the JSON string.
        return text_preview(&raw, max_chars);
    }
}

/// Load JSONL trace events from a file (skips blank / non-object lines).
pub fn load_trace_file(path: impl AsRef<Path>) -> std::io::Result<Vec<TraceEvent>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TraceEvent>(line) {
            Ok(ev) => out.push(ev),
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("line {}: {e}", i + 1),
                ));
            }
        }
    }
    Ok(out)
}

/// Aggregated metrics from a single run's events.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TraceStats {
    pub run_id: Option<String>,
    pub status: Option<TraceRunStatus>,
    pub turns: usize,
    pub wall_ms: u64,
    pub llm_calls: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub gate_denies: usize,
    pub gate_rewrites: usize,
    pub total_llm_latency_ms: u64,
    pub total_tool_duration_ms: u64,
    pub ttft_samples_ms: Vec<u64>,
    pub usage: TokenUsage,
    pub tool_names: Vec<String>,
    pub pass: Option<bool>,
    pub score: Option<f64>,
}

impl TraceStats {
    pub fn from_events(events: &[TraceEvent]) -> Self {
        let mut s = Self::default();
        for ev in events {
            match ev {
                TraceEvent::RunStart { run_id, .. } => {
                    s.run_id = Some(run_id.clone());
                }
                TraceEvent::RunEnd {
                    status,
                    turns,
                    wall_ms,
                    usage,
                    ..
                } => {
                    s.status = Some(status.clone());
                    s.turns = *turns;
                    s.wall_ms = *wall_ms;
                    s.usage = *usage;
                }
                TraceEvent::LlmResponse {
                    latency_ms,
                    ttft_ms,
                    usage,
                    ..
                } => {
                    s.llm_calls += 1;
                    s.total_llm_latency_ms = s.total_llm_latency_ms.saturating_add(*latency_ms);
                    if let Some(t) = ttft_ms {
                        s.ttft_samples_ms.push(*t);
                    }
                    // Prefer cumulative usage from RunEnd; if missing, sum responses.
                    if s.usage.is_zero() {
                        s.usage.add_assign(usage);
                    }
                }
                TraceEvent::ToolStart { name, .. } => {
                    s.tool_calls += 1;
                    s.tool_names.push(name.clone());
                }
                TraceEvent::ToolEnd {
                    duration_ms,
                    is_error,
                    ..
                } => {
                    s.total_tool_duration_ms =
                        s.total_tool_duration_ms.saturating_add(*duration_ms);
                    if *is_error {
                        s.tool_errors += 1;
                    }
                }
                TraceEvent::Gate { decision, .. } => match decision {
                    TraceGateDecision::Deny => s.gate_denies += 1,
                    TraceGateDecision::Rewrite => s.gate_rewrites += 1,
                    TraceGateDecision::Allow => {}
                },
                TraceEvent::Score { pass, score, .. } => {
                    s.pass = Some(*pass);
                    s.score = Some(*score);
                }
                _ => {}
            }
        }
        // If RunEnd had usage, keep it; else we may have summed LlmResponse above.
        // When RunEnd set usage, LlmResponse path was skipped once non-zero — good.
        // Re-sum from responses if RunEnd usage zero but we saw responses with usage.
        if s.usage.is_zero() {
            let mut u = TokenUsage::default();
            for ev in events {
                if let TraceEvent::LlmResponse { usage, .. } = ev {
                    u.add_assign(usage);
                }
            }
            s.usage = u;
        }
        s
    }

    pub fn tool_error_rate(&self) -> f64 {
        if self.tool_calls == 0 {
            0.0
        } else {
            self.tool_errors as f64 / self.tool_calls as f64
        }
    }

    pub fn avg_llm_latency_ms(&self) -> Option<f64> {
        if self.llm_calls == 0 {
            None
        } else {
            Some(self.total_llm_latency_ms as f64 / self.llm_calls as f64)
        }
    }

    pub fn ttft_p50_ms(&self) -> Option<u64> {
        if self.ttft_samples_ms.is_empty() {
            return None;
        }
        let mut v = self.ttft_samples_ms.clone();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    /// Human-readable multi-line summary.
    pub fn format_report(&self) -> String {
        let mut lines = Vec::new();
        if let Some(id) = &self.run_id {
            lines.push(format!("run_id:     {id}"));
        }
        if let Some(st) = &self.status {
            lines.push(format!("status:     {st:?}"));
        }
        lines.push(format!("turns:      {}", self.turns));
        lines.push(format!("wall_ms:    {}", self.wall_ms));
        lines.push(format!("llm_calls:  {}", self.llm_calls));
        if let Some(avg) = self.avg_llm_latency_ms() {
            lines.push(format!("llm_lat_avg_ms: {avg:.1}"));
        }
        if let Some(p50) = self.ttft_p50_ms() {
            lines.push(format!("ttft_p50_ms:    {p50}"));
        }
        lines.push(format!(
            "tool_calls: {} (errors={}, rate={:.0}%)",
            self.tool_calls,
            self.tool_errors,
            self.tool_error_rate() * 100.0
        ));
        lines.push(format!("tool_ms:    {}", self.total_tool_duration_ms));
        if self.gate_denies > 0 || self.gate_rewrites > 0 {
            lines.push(format!(
                "gates:      denies={} rewrites={}",
                self.gate_denies, self.gate_rewrites
            ));
        }
        if !self.usage.is_zero() {
            lines.push(format!(
                "tokens:     in={} out={} cache_r={} cache_w={} total={}",
                self.usage.input_tokens,
                self.usage.output_tokens,
                self.usage.cache_read_tokens,
                self.usage.cache_write_tokens,
                self.usage.total()
            ));
        }
        if !self.tool_names.is_empty() {
            lines.push(format!("tools:      {}", self.tool_names.join(", ")));
        }
        if let Some(pass) = self.pass {
            lines.push(format!(
                "score:      pass={pass} score={}",
                self.score.unwrap_or(0.0)
            ));
        }
        lines.join("\n")
    }
}

/// Generate a short run id without extra deps (hex of millis + counter-ish).
pub fn new_run_id() -> String {
    let ms = now_ms();
    // Mix in a pseudo-randomish low bits from address of a stack value.
    let salt = &ms as *const u64 as usize;
    format!("run_{ms:x}_{salt:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_trace_records() {
        let t = MemoryTrace::new();
        t.record(TraceEvent::RunStart {
            ts_ms: 1,
            run_id: "r1".into(),
            agent: "one".into(),
            agent_version: None,
            provider: None,
            model: None,
            task_id: None,
            config: None,
            session_id: None,
            user_id: None,
            trace_full: false,
            input_preview: None,
        });
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn jsonl_roundtrip() {
        let dir = std::env::temp_dir().join(format!("one-trace-test-{}", now_ms()));
        let path = dir.join("t.jsonl");
        let sink = JsonlTraceSink::create(&path).unwrap();
        sink.record(TraceEvent::ToolStart {
            ts_ms: 1,
            run_id: "r".into(),
            turn: 0,
            call_id: "c1".into(),
            name: "bash".into(),
            args_bytes: 10,
            args_preview: Some(r#"{"command":"ls"}"#.into()),
        });
        sink.flush().unwrap();
        let events = load_trace_file(&path).unwrap();
        assert_eq!(events.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stats_from_events() {
        let events = vec![
            TraceEvent::RunStart {
                ts_ms: 0,
                run_id: "r".into(),
                agent: "one".into(),
                agent_version: None,
                provider: Some("mock".into()),
                model: Some("m".into()),
                task_id: None,
                config: None,
                session_id: Some("sess-1".into()),
                user_id: None,
                trace_full: false,
                input_preview: Some("hi".into()),
            },
            TraceEvent::LlmResponse {
                ts_ms: 10,
                run_id: "r".into(),
                turn: 0,
                latency_ms: 100,
                ttft_ms: Some(20),
                stop_reason: "tool_use".into(),
                tool_calls_n: 1,
                text_len: 0,
                thinking_len: 0,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                provider: "mock".into(),
                model: "m".into(),
                output_preview: None,
                tool_calls: vec![TraceToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: Some(json!({"command": "ls"})),
                }],
            },
            TraceEvent::ToolStart {
                ts_ms: 11,
                run_id: "r".into(),
                turn: 0,
                call_id: "1".into(),
                name: "bash".into(),
                args_bytes: 5,
                args_preview: None,
            },
            TraceEvent::ToolEnd {
                ts_ms: 21,
                run_id: "r".into(),
                turn: 0,
                call_id: "1".into(),
                name: "bash".into(),
                duration_ms: 10,
                is_error: false,
                output_bytes: 20,
                gate: Some(TraceGateDecision::Allow),
                output_preview: None,
            },
            TraceEvent::RunEnd {
                ts_ms: 30,
                run_id: "r".into(),
                status: TraceRunStatus::Ok,
                turns: 1,
                wall_ms: 30,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                final_text_len: Some(12),
                final_text_preview: Some("hello".into()),
                error: None,
            },
        ];
        let s = TraceStats::from_events(&events);
        assert_eq!(s.turns, 1);
        assert_eq!(s.tool_calls, 1);
        assert_eq!(s.llm_calls, 1);
        assert_eq!(s.usage.total(), 15);
        assert_eq!(s.ttft_p50_ms(), Some(20));
    }

    #[test]
    fn args_preview_truncates() {
        let (n, p) = args_preview(&json!({"x": "hello world"}), 8);
        assert!(n > 8);
        assert!(p.unwrap().ends_with('…'));
    }

    #[test]
    fn llm_output_preview_structured_text() {
        let p = llm_output_preview("你好！有什么我可以帮你的吗？", &[], 240).unwrap();
        let v: Value = serde_json::from_str(&p).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(v["content"].as_str().unwrap().contains("你好"));
        assert!(v.get("tool_calls").is_none());
    }

    #[test]
    fn llm_output_preview_structured_tool_calls() {
        let calls = vec![crate::tool::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        }];
        let p = llm_output_preview("", &calls, 1024).unwrap();
        let v: Value = serde_json::from_str(&p).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(v["content"].is_null());
        assert_eq!(v["tool_calls"][0]["id"], "1");
        assert_eq!(v["tool_calls"][0]["name"], "bash");
        assert_eq!(v["tool_calls"][0]["arguments"]["command"], "ls");
    }

    #[test]
    fn llm_output_preview_text_plus_tools() {
        let calls = vec![crate::tool::ToolCall {
            id: "tc".into(),
            name: "read".into(),
            arguments: json!({"path": "README.md"}),
        }];
        let p = llm_output_preview("先看 README", &calls, 1024).unwrap();
        let v: Value = serde_json::from_str(&p).unwrap();
        assert_eq!(v["content"], "先看 README");
        assert_eq!(v["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn last_user_preview_takes_latest() {
        use crate::message::{AgentMessage, UserContent, UserMessage};
        let messages = vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Text("first".into()),
                timestamp: 0,
            }),
            AgentMessage::User(UserMessage {
                content: UserContent::Text("second".into()),
                timestamp: 1,
            }),
        ];
        assert_eq!(last_user_preview(&messages, 240).as_deref(), Some("second"));
    }

    #[test]
    fn llm_input_preview_includes_full_conversation() {
        use crate::message::{
            AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage,
            UserContent, UserMessage,
        };
        let messages = vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Text("这个项目干啥的".into()),
                timestamp: 0,
            }),
            AgentMessage::Assistant(AssistantMessage {
                content: vec![
                    ContentBlock::Text {
                        text: "先看结构".into(),
                    },
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "ls".into(),
                        arguments: json!({}),
                    },
                ],
                provider: "mock".into(),
                model: "m".into(),
                stop_reason: StopReason::ToolUse,
                citations: vec![],
                timestamp: 1,
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "ls".into(),
                content: vec![crate::message::TextOrImage::Text {
                    text: "Cargo.toml\nREADME.md".into(),
                }],
                is_error: false,
                timestamp: 2,
            }),
        ];
        let p = llm_input_preview("you are one", &messages, PREVIEW_LLM_CHARS).unwrap();
        let v: Value = serde_json::from_str(&p).unwrap();
        let arr = v.as_array().expect("messages array");
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "you are one");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[1]["content"], "这个项目干啥的");
        assert_eq!(arr[2]["role"], "assistant");
        assert_eq!(arr[2]["content"], "先看结构");
        assert_eq!(arr[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(arr[2]["tool_calls"][0]["name"], "ls");
        assert_eq!(arr[3]["role"], "tool");
        assert_eq!(arr[3]["tool_call_id"], "c1");
        assert!(arr[3]["content"].as_str().unwrap().contains("Cargo.toml"));
    }

    #[test]
    fn llm_input_preview_not_just_last_user() {
        use crate::message::{AgentMessage, UserContent, UserMessage};
        let messages = vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Text("turn0".into()),
                timestamp: 0,
            }),
            AgentMessage::User(UserMessage {
                content: UserContent::Text("turn1".into()),
                timestamp: 1,
            }),
        ];
        let p = llm_input_preview("sys", &messages, 4096).unwrap();
        assert!(p.contains("turn0"), "must keep earlier user turns: {p}");
        assert!(p.contains("turn1"));
        assert!(p.contains("\"role\":\"system\"") || p.contains("\"role\": \"system\""));
    }

    #[test]
    fn llm_input_preview_truncates_huge_tool_results() {
        use crate::message::{
            AgentMessage, ToolResultMessage, UserContent, UserMessage,
        };
        let huge = "x".repeat(20_000);
        let messages = vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Text("q".into()),
                timestamp: 0,
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "read".into(),
                content: vec![crate::message::TextOrImage::Text { text: huge }],
                is_error: false,
                timestamp: 1,
            }),
        ];
        let p = llm_input_preview("sys", &messages, 8_192).unwrap();
        assert!(p.chars().count() <= 8_192);
        assert!(p.contains("tool_call_id"));
    }
}
