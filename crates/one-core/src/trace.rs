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
use crate::message::now_ms;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreCheckResult {
    pub name: String,
    pub pass: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    if max_chars == 0 {
        return (bytes, None);
    }
    let preview = if raw.chars().count() <= max_chars {
        raw
    } else {
        let truncated: String = raw.chars().take(max_chars).collect();
        format!("{truncated}…")
    };
    (bytes, Some(preview))
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
                    s.total_llm_latency_ms =
                        s.total_llm_latency_ms.saturating_add(*latency_ms);
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
        lines.push(format!(
            "tool_ms:    {}",
            self.total_tool_duration_ms
        ));
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
}
