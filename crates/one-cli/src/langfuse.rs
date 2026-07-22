//! Langfuse observability via **OpenTelemetry OTLP** (official path for non-SDK languages).
//!
//! ## Transport
//!
//! ```text
//! POST {LANGFUSE_BASE_URL}/api/public/otel/v1/traces   (OTLP/HTTP protobuf)
//! Authorization: Basic base64(pk:sk)
//! x-langfuse-ingestion-version: 4
//! ```
//!
//! Docs:
//! - <https://langfuse.com/integrations/native/opentelemetry>
//! - <https://langfuse.com/docs/observability/data-model>
//!
//! ## Env (official)
//!
//! - `LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY` (required)
//! - `LANGFUSE_BASE_URL` (preferred) or `LANGFUSE_HOST`
//! - `LANGFUSE_TRACING_ENVIRONMENT` → resource / span `langfuse.environment`
//! - `LANGFUSE_RELEASE` → `langfuse.release`
//! - `LANGFUSE_USER_ID` / `ONE_USER_ID` → `langfuse.user.id` (optional)
//! - `ONE_LANGFUSE=0` disables
//!
//! ## Best-practice notes
//!
//! - Trace-level attrs (`session`, `user`, `tags`, `metadata`, `release`, `env`)
//!   are **propagated to every span** so Langfuse filters/aggregations work.
//! - `langfuse.trace.tags` is emitted as an OTEL string **array**.
//! - Scores: `force_flush` OTLP first, then POST Scores API; `shutdown` joins workers.
//!
//! Short-lived CLI: call [`LangfuseTraceSink::shutdown`] before process exit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use one_core::{TraceEvent, TraceGateDecision, TraceRunStatus, TraceSink};
use opentelemetry::trace::{Span, SpanKind, Status, TraceContextExt, Tracer, TracerProvider as _};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use serde_json::json;

const SERVICE_NAME: &str = "one";
const TRACER_NAME: &str = "one-agent";

/// Resolved Langfuse project credentials + host.
#[derive(Debug, Clone)]
pub struct LangfuseConfig {
    pub public_key: String,
    pub secret_key: String,
    /// Base URL without trailing slash, e.g. `https://us.cloud.langfuse.com`.
    pub base_url: String,
}

impl LangfuseConfig {
    pub fn from_env() -> Option<Self> {
        if env_disabled("ONE_LANGFUSE") {
            return None;
        }
        let public_key = std::env::var("LANGFUSE_PUBLIC_KEY").ok()?;
        let secret_key = std::env::var("LANGFUSE_SECRET_KEY").ok()?;
        if public_key.trim().is_empty() || secret_key.trim().is_empty() {
            return None;
        }
        let base_url = std::env::var("LANGFUSE_BASE_URL")
            .or_else(|_| std::env::var("LANGFUSE_HOST"))
            .unwrap_or_else(|_| "https://cloud.langfuse.com".into());
        Some(Self {
            public_key: public_key.trim().to_string(),
            secret_key: secret_key.trim().to_string(),
            base_url: base_url.trim().trim_end_matches('/').to_string(),
        })
    }

    /// Full OTLP/HTTP traces URL (rust exporter does not always append `/v1/traces`).
    /// Official signal endpoint: `{host}/api/public/otel/v1/traces`
    pub fn otlp_endpoint(&self) -> String {
        format!("{}/api/public/otel/v1/traces", self.base_url)
    }

    pub fn otlp_traces_url(&self) -> String {
        self.otlp_endpoint()
    }

    pub fn scores_url(&self) -> String {
        format!("{}/api/public/scores", self.base_url)
    }

    pub fn project_url_hint(&self) -> String {
        self.base_url.clone()
    }

    pub fn basic_auth_header(&self) -> String {
        use base64::Engine;
        let raw = format!("{}:{}", self.public_key, self.secret_key);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
        )
    }
}

fn env_disabled(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| {
        let s = v.to_string_lossy();
        s == "0" || s.eq_ignore_ascii_case("false") || s.eq_ignore_ascii_case("off")
    })
}

fn tracing_environment() -> Option<String> {
    std::env::var("LANGFUSE_TRACING_ENVIRONMENT")
        .or_else(|_| std::env::var("ONE_ENV"))
        .ok()
        .filter(|s| !s.is_empty())
}

fn release() -> String {
    std::env::var("LANGFUSE_RELEASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Optional user id from env (wired into every span when present).
pub fn user_id_from_env() -> Option<String> {
    std::env::var("LANGFUSE_USER_ID")
        .or_else(|_| std::env::var("ONE_USER_ID"))
        .or_else(|_| std::env::var("USER"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn ms_to_system_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn tags_attr(tags: &[String]) -> KeyValue {
    // Own the strings so the OTEL attribute does not borrow from temporaries.
    let values: Vec<StringValue> = tags.iter().cloned().map(StringValue::from).collect();
    KeyValue::new("langfuse.trace.tags", Value::Array(Array::String(values)))
}

/// Build attrs that must appear on **every** span for Langfuse filter/aggregate.
fn build_propagated(
    session_id: Option<&str>,
    user_id: Option<&str>,
    task_id: Option<&str>,
    agent_version: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    config: Option<&serde_json::Value>,
    trace_name: &str,
) -> Vec<KeyValue> {
    let mut tags = vec!["one".to_string(), "agent".to_string()];
    if let Some(t) = task_id {
        tags.push(format!("task:{t}"));
    }

    let mut attrs = vec![
        tags_attr(&tags),
        KeyValue::new("langfuse.trace.name", trace_name.to_string()),
        KeyValue::new("langfuse.release", release()),
        KeyValue::new(
            "langfuse.version",
            agent_version.unwrap_or_default().to_string(),
        ),
    ];
    if let Some(env) = tracing_environment() {
        attrs.push(KeyValue::new("langfuse.environment", env));
    }
    if let Some(sid) = session_id {
        attrs.push(KeyValue::new("langfuse.session.id", sid.to_string()));
        attrs.push(KeyValue::new("session.id", sid.to_string()));
    }
    if let Some(uid) = user_id {
        attrs.push(KeyValue::new("langfuse.user.id", uid.to_string()));
        attrs.push(KeyValue::new("user.id", uid.to_string()));
    }
    if let Some(t) = task_id {
        attrs.push(KeyValue::new(
            "langfuse.trace.metadata.task_id",
            t.to_string(),
        ));
    }
    if let Some(p) = provider {
        attrs.push(KeyValue::new(
            "langfuse.trace.metadata.provider",
            p.to_string(),
        ));
    }
    if let Some(m) = model {
        attrs.push(KeyValue::new(
            "langfuse.trace.metadata.model",
            m.to_string(),
        ));
    }
    // Flatten config object keys for filterable top-level metadata.
    if let Some(cfg) = config {
        if let Some(obj) = cfg.as_object() {
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                attrs.push(KeyValue::new(format!("langfuse.trace.metadata.{k}"), val));
            }
        } else {
            attrs.push(KeyValue::new(
                "langfuse.trace.metadata.config",
                cfg.to_string(),
            ));
        }
    }
    attrs
}

/// In-flight OTEL contexts for one agent run (parent → child nesting).
#[derive(Default)]
struct RunState {
    run_id: Option<String>,
    /// Hex OTEL trace id (Langfuse trace id for OTLP-ingested traces).
    otel_trace_id: Option<String>,
    /// Root agent span context.
    root: Option<Context>,
    turns: HashMap<usize, Context>,
    llms: HashMap<String, Context>,
    tools: HashMap<String, Context>,
    /// Copied onto every child span (session/user/tags/metadata/…).
    propagated: Vec<KeyValue>,
}

/// TraceSink exporting to Langfuse over OTLP/HTTP.
pub struct LangfuseTraceSink {
    config: LangfuseConfig,
    tracer: opentelemetry_sdk::trace::Tracer,
    provider: Mutex<Option<SdkTracerProvider>>,
    events: Mutex<Vec<TraceEvent>>,
    state: Mutex<RunState>,
    http: reqwest::blocking::Client,
    /// Score HTTP workers; joined during [`Self::shutdown`] after OTLP flush.
    pending_scores: Mutex<Vec<JoinHandle<()>>>,
    /// True when a real OTLP exporter is attached (false for memory-only fallback).
    exporting: bool,
}

impl LangfuseTraceSink {
    pub fn start(config: LangfuseConfig) -> Arc<Self> {
        match Self::try_start(config.clone()) {
            Ok(sink) => sink,
            Err(e) => {
                eprintln!("langfuse: failed to init OTEL exporter: {e}");
                tracing::error!(error = %e, "langfuse OTEL init failed");
                // Fall back to a no-export sink that still buffers events for scoring.
                Self::memory_only(config)
            }
        }
    }

    fn memory_only(config: LangfuseConfig) -> Arc<Self> {
        // Minimal provider with no exporter — record() still stores events.
        let provider = SdkTracerProvider::builder()
            .with_resource(Resource::builder().with_service_name(SERVICE_NAME).build())
            .build();
        let tracer = provider.tracer(TRACER_NAME);
        Arc::new(Self {
            config,
            tracer,
            provider: Mutex::new(Some(provider)),
            events: Mutex::new(Vec::new()),
            state: Mutex::new(RunState::default()),
            http: reqwest::blocking::Client::new(),
            pending_scores: Mutex::new(Vec::new()),
            exporting: false,
        })
    }

    fn try_start(config: LangfuseConfig) -> Result<Arc<Self>, String> {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), config.basic_auth_header());
        // Fast Preview / current ingestion pipeline
        headers.insert("x-langfuse-ingestion-version".to_string(), "4".to_string());

        let exporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(config.otlp_endpoint())
            .with_protocol(Protocol::HttpBinary)
            .with_headers(headers)
            .with_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;

        let mut resource_kvs = vec![
            KeyValue::new("service.name", SERVICE_NAME),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("langfuse.release", release()),
        ];
        if let Some(env) = tracing_environment() {
            resource_kvs.push(KeyValue::new("langfuse.environment", env));
        }

        // Batch processor bound to the Tokio runtime (async OTLP HTTP client).
        // Short-lived CLI still force_flush on shutdown.
        let processor = BatchSpanProcessor::builder(exporter, Tokio).build();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(processor)
            .with_resource(Resource::builder().with_attributes(resource_kvs).build())
            .build();

        let tracer = provider.tracer(TRACER_NAME);
        // reqwest::blocking builds its own Tokio runtime — must not construct
        // it on an async worker (panics). Build on a dedicated thread.
        let http = std::thread::Builder::new()
            .name("langfuse-http-init".into())
            .spawn(|| {
                reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(15))
                    .user_agent(format!("one-rust/{}", env!("CARGO_PKG_VERSION")))
                    .build()
            })
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "langfuse http client thread panicked".to_string())?
            .map_err(|e| e.to_string())?;

        Ok(Arc::new(Self {
            config,
            tracer,
            provider: Mutex::new(Some(provider)),
            events: Mutex::new(Vec::new()),
            state: Mutex::new(RunState::default()),
            http,
            pending_scores: Mutex::new(Vec::new()),
            exporting: true,
        }))
    }

    pub fn events(&self) -> Vec<TraceEvent> {
        self.events.lock().expect("events").clone()
    }

    /// Force-flush pending OTLP spans without shutting the provider down.
    pub fn flush(&self) {
        if let Ok(guard) = self.provider.lock() {
            if let Some(provider) = guard.as_ref() {
                if let Err(e) = provider.force_flush() {
                    tracing::warn!(error = %e, "langfuse: force_flush failed");
                }
            }
        }
    }

    fn join_pending_scores(&self) {
        let handles = self
            .pending_scores
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        for h in handles {
            if let Err(e) = h.join() {
                tracing::warn!("langfuse: score worker panicked: {e:?}");
            }
        }
    }

    /// Force-flush OTLP, wait for score workers, and shut down the provider (idempotent).
    ///
    /// OTLP HTTP uses a blocking reqwest client; flush/shutdown must not run on a
    /// Tokio worker thread (would panic: "Cannot drop a runtime in async context").
    pub fn shutdown(&self) {
        // End any open spans best-effort.
        if let Ok(mut st) = self.state.lock() {
            for (_, cx) in st.tools.drain() {
                cx.span().end();
            }
            for (_, cx) in st.llms.drain() {
                cx.span().end();
            }
            for (_, cx) in st.turns.drain() {
                cx.span().end();
            }
            if let Some(cx) = st.root.take() {
                cx.span().end();
            }
            st.run_id = None;
            // Keep otel_trace_id for any late scores; cleared on next RunStart.
        }

        // 1) Export spans so Langfuse has the trace before score linkage.
        self.flush();

        // 2) Wait for score HTTP posts (posted after flush on Score events, or earlier).
        self.join_pending_scores();

        // 3) Tear down provider.
        let provider = match self.provider.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        if let Some(provider) = provider {
            if let Err(e) = provider.force_flush() {
                tracing::warn!(error = %e, "langfuse: force_flush failed");
                eprintln!("langfuse: force_flush failed: {e}");
            }
            if let Err(e) = provider.shutdown() {
                tracing::warn!(error = %e, "langfuse: provider shutdown failed");
                eprintln!("langfuse: provider shutdown failed: {e}");
            }
        }
    }

    fn parent_cx(state: &RunState, turn: Option<usize>) -> Context {
        if let Some(t) = turn {
            if let Some(cx) = state.turns.get(&t) {
                return cx.clone();
            }
        }
        state.root.clone().unwrap_or_else(Context::current)
    }

    fn with_propagated(state: &RunState, mut attrs: Vec<KeyValue>) -> Vec<KeyValue> {
        attrs.extend(state.propagated.iter().cloned());
        attrs
    }

    fn apply_event(&self, event: &TraceEvent) {
        let mut state = self.state.lock().expect("state");
        match event {
            TraceEvent::RunStart {
                ts_ms,
                run_id,
                agent,
                agent_version,
                provider,
                model,
                task_id,
                config,
                session_id,
                user_id,
                trace_full: _,
            } => {
                // Close previous run if any.
                if let Some(cx) = state.root.take() {
                    cx.span().end();
                }
                state.turns.clear();
                state.llms.clear();
                state.tools.clear();
                state.run_id = Some(run_id.clone());
                state.otel_trace_id = None;

                let name = match task_id {
                    Some(t) => format!("one:{t}"),
                    None => format!("one:{agent}"),
                };

                state.propagated = build_propagated(
                    session_id.as_deref(),
                    user_id.as_deref(),
                    task_id.as_deref(),
                    agent_version.as_deref(),
                    provider.as_deref(),
                    model.as_deref(),
                    config.as_ref(),
                    &name,
                );

                let mut attrs = vec![
                    KeyValue::new("langfuse.observation.type", "agent"),
                    KeyValue::new("agent", agent.clone()),
                    KeyValue::new("run_id", run_id.clone()),
                    KeyValue::new("langfuse.trace.metadata.run_id", run_id.clone()),
                ];
                if let Some(p) = provider {
                    attrs.push(KeyValue::new("provider", p.clone()));
                }
                if let Some(m) = model {
                    attrs.push(KeyValue::new("model", m.clone()));
                }
                if let Some(v) = agent_version {
                    attrs.push(KeyValue::new("agent_version", v.clone()));
                }
                attrs = Self::with_propagated(&state, attrs);

                let span = self
                    .tracer
                    .span_builder(name)
                    .with_kind(SpanKind::Internal)
                    .with_start_time(ms_to_system_time(*ts_ms))
                    .with_attributes(attrs)
                    .start(&self.tracer);
                let cx = Context::current_with_span(span);
                let tid = cx.span().span_context().trace_id().to_string();
                state.otel_trace_id = Some(tid);
                state.root = Some(cx);
            }
            TraceEvent::RunEnd {
                ts_ms,
                run_id,
                status,
                turns,
                wall_ms,
                usage,
                final_text_len,
                final_text_preview,
                error,
            } => {
                let status_str = format!("{status:?}").to_lowercase();
                // Close nested spans
                for (_, cx) in state.tools.drain() {
                    cx.span().end_with_timestamp(ms_to_system_time(*ts_ms));
                }
                for (_, cx) in state.llms.drain() {
                    cx.span().end_with_timestamp(ms_to_system_time(*ts_ms));
                }
                for (_, cx) in state.turns.drain() {
                    cx.span().end_with_timestamp(ms_to_system_time(*ts_ms));
                }
                if let Some(cx) = state.root.take() {
                    let span = cx.span();
                    span.set_attribute(KeyValue::new("status", status_str.clone()));
                    span.set_attribute(KeyValue::new("turns", *turns as i64));
                    span.set_attribute(KeyValue::new("wall_ms", *wall_ms as i64));
                    if let Some(n) = final_text_len {
                        span.set_attribute(KeyValue::new("final_text_len", *n as i64));
                    }
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.input_tokens",
                        usage.input_tokens as i64,
                    ));
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.output_tokens",
                        usage.output_tokens as i64,
                    ));
                    let output = if let Some(preview) = final_text_preview {
                        if error.is_some() {
                            json!({"error": error, "status": status_str, "text": preview})
                                .to_string()
                        } else {
                            preview.clone()
                        }
                    } else {
                        error
                            .as_ref()
                            .map(|e| json!({"error": e, "status": status_str}).to_string())
                            .unwrap_or_else(|| json!({"status": status_str}).to_string())
                    };
                    span.set_attribute(KeyValue::new("langfuse.observation.output", output));
                    // Also set trace-level output for the whole run.
                    if let Some(preview) = final_text_preview {
                        span.set_attribute(KeyValue::new("langfuse.trace.output", preview.clone()));
                    }
                    match status {
                        TraceRunStatus::Ok => span.set_status(Status::Ok),
                        TraceRunStatus::Error => span.set_status(Status::error(
                            error.clone().unwrap_or_else(|| "run error".into()),
                        )),
                        TraceRunStatus::Aborted | TraceRunStatus::MaxTurns => {
                            span.set_status(Status::error(status_str));
                        }
                    }
                    span.end_with_timestamp(ms_to_system_time(*ts_ms));
                }
                let _ = run_id;
                state.run_id = None;
                // keep otel_trace_id until scores are posted (Score may come after RunEnd)
            }
            TraceEvent::TurnStart {
                ts_ms,
                run_id: _,
                turn,
                message_count,
                tools_n,
                last_prompt_tokens,
            } => {
                let parent = Self::parent_cx(&state, None);
                let mut attrs = vec![
                    KeyValue::new("langfuse.observation.type", "chain"),
                    KeyValue::new("turn", *turn as i64),
                    KeyValue::new("message_count", *message_count as i64),
                    KeyValue::new("tools_n", *tools_n as i64),
                ];
                attrs = Self::with_propagated(&state, attrs);
                let mut span = self
                    .tracer
                    .span_builder(format!("turn-{turn}"))
                    .with_kind(SpanKind::Internal)
                    .with_start_time(ms_to_system_time(*ts_ms))
                    .with_attributes(attrs)
                    .start_with_context(&self.tracer, &parent);
                if let Some(p) = last_prompt_tokens {
                    span.set_attribute(KeyValue::new("last_prompt_tokens", *p as i64));
                }
                state.turns.insert(*turn, Context::current_with_span(span));
            }
            TraceEvent::LlmRequest {
                ts_ms,
                run_id,
                turn,
                message_count,
                tools_n,
                system_prompt_len,
                input_preview,
            } => {
                let parent = Self::parent_cx(&state, Some(*turn));
                let key = format!("{run_id}:{turn}");
                let mut attrs = vec![
                    KeyValue::new("langfuse.observation.type", "generation"),
                    KeyValue::new("turn", *turn as i64),
                    KeyValue::new("message_count", *message_count as i64),
                    KeyValue::new("tools_n", *tools_n as i64),
                    KeyValue::new("system_prompt_len", *system_prompt_len as i64),
                ];
                if let Some(preview) = input_preview {
                    attrs.push(KeyValue::new("langfuse.observation.input", preview.clone()));
                }
                attrs = Self::with_propagated(&state, attrs);
                let span = self
                    .tracer
                    .span_builder(format!("llm-turn-{turn}"))
                    .with_kind(SpanKind::Client)
                    .with_start_time(ms_to_system_time(*ts_ms))
                    .with_attributes(attrs)
                    .start_with_context(&self.tracer, &parent);
                state.llms.insert(key, Context::current_with_span(span));
            }
            TraceEvent::LlmResponse {
                ts_ms,
                run_id,
                turn,
                latency_ms,
                ttft_ms,
                stop_reason,
                tool_calls_n,
                text_len,
                thinking_len,
                usage,
                provider,
                model,
                output_preview,
            } => {
                let key = format!("{run_id}:{turn}");
                if let Some(cx) = state.llms.remove(&key) {
                    let span = cx.span();
                    span.set_attribute(KeyValue::new("langfuse.observation.type", "generation"));
                    span.set_attribute(KeyValue::new("gen_ai.request.model", model.clone()));
                    span.set_attribute(KeyValue::new("gen_ai.response.model", model.clone()));
                    span.set_attribute(KeyValue::new(
                        "langfuse.observation.model.name",
                        model.clone(),
                    ));
                    span.set_attribute(KeyValue::new("gen_ai.system", provider.clone()));
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.input_tokens",
                        usage.input_tokens as i64,
                    ));
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.output_tokens",
                        usage.output_tokens as i64,
                    ));
                    // Langfuse usageDetails JSON (preferred over deprecated usage)
                    let mut usage_details = json!({
                        "input": usage.input_tokens,
                        "output": usage.output_tokens,
                        "total": usage.total(),
                    });
                    if usage.cache_read_tokens > 0 {
                        usage_details["cache_read_input_tokens"] = json!(usage.cache_read_tokens);
                        span.set_attribute(KeyValue::new(
                            "gen_ai.usage.cache_read_input_tokens",
                            usage.cache_read_tokens as i64,
                        ));
                    }
                    if usage.cache_write_tokens > 0 {
                        usage_details["cache_creation_input_tokens"] =
                            json!(usage.cache_write_tokens);
                    }
                    span.set_attribute(KeyValue::new(
                        "langfuse.observation.usage_details",
                        usage_details.to_string(),
                    ));
                    span.set_attribute(KeyValue::new("latency_ms", *latency_ms as i64));
                    if let Some(t) = ttft_ms {
                        span.set_attribute(KeyValue::new("ttft_ms", *t as i64));
                        let start = ts_ms.saturating_sub(*latency_ms);
                        let completion = ms_to_system_time(start.saturating_add(*t));
                        // ISO completion start as string attribute (Langfuse maps this)
                        if let Ok(dur) = completion.duration_since(UNIX_EPOCH) {
                            let iso = format_iso_ms(dur.as_millis() as u64);
                            span.set_attribute(KeyValue::new(
                                "langfuse.observation.completion_start_time",
                                iso,
                            ));
                        }
                    }
                    span.set_attribute(KeyValue::new("stop_reason", stop_reason.clone()));
                    span.set_attribute(KeyValue::new("tool_calls_n", *tool_calls_n as i64));
                    span.set_attribute(KeyValue::new("text_len", *text_len as i64));
                    span.set_attribute(KeyValue::new("thinking_len", *thinking_len as i64));
                    if let Some(preview) = output_preview {
                        span.set_attribute(KeyValue::new(
                            "langfuse.observation.output",
                            preview.clone(),
                        ));
                    } else {
                        span.set_attribute(KeyValue::new(
                            "langfuse.observation.output",
                            json!({
                                "stop_reason": stop_reason,
                                "tool_calls_n": tool_calls_n,
                                "text_len": text_len,
                            })
                            .to_string(),
                        ));
                    }
                    span.set_status(Status::Ok);
                    span.end_with_timestamp(ms_to_system_time(*ts_ms));
                }
                // Close turn span end time after LLM (tools may still run under same turn)
                if let Some(cx) = state.turns.get(turn) {
                    // don't end yet if tools may follow — leave open until RunEnd or next turn
                    let _ = cx;
                }
            }
            TraceEvent::ToolStart {
                ts_ms,
                run_id: _,
                turn,
                call_id,
                name,
                args_bytes,
                args_preview,
            } => {
                let parent = Self::parent_cx(&state, Some(*turn));
                let mut attrs = vec![
                    KeyValue::new("langfuse.observation.type", "tool"),
                    KeyValue::new("tool.name", name.clone()),
                    KeyValue::new("call_id", call_id.clone()),
                    KeyValue::new("turn", *turn as i64),
                    KeyValue::new("args_bytes", *args_bytes as i64),
                ];
                if let Some(p) = args_preview {
                    attrs.push(KeyValue::new(
                        "langfuse.observation.input",
                        json!({"args_preview": p, "args_bytes": args_bytes}).to_string(),
                    ));
                }
                attrs = Self::with_propagated(&state, attrs);
                let span = self
                    .tracer
                    .span_builder(name.clone())
                    .with_kind(SpanKind::Internal)
                    .with_start_time(ms_to_system_time(*ts_ms))
                    .with_attributes(attrs)
                    .start_with_context(&self.tracer, &parent);
                state
                    .tools
                    .insert(call_id.clone(), Context::current_with_span(span));
            }
            TraceEvent::ToolEnd {
                ts_ms,
                run_id: _,
                turn,
                call_id,
                name,
                duration_ms,
                is_error,
                output_bytes,
                gate,
                output_preview,
            } => {
                if let Some(cx) = state.tools.remove(call_id) {
                    let span = cx.span();
                    span.set_attribute(KeyValue::new("langfuse.observation.type", "tool"));
                    span.set_attribute(KeyValue::new("tool.name", name.clone()));
                    span.set_attribute(KeyValue::new("duration_ms", *duration_ms as i64));
                    span.set_attribute(KeyValue::new("output_bytes", *output_bytes as i64));
                    span.set_attribute(KeyValue::new("is_error", *is_error));
                    span.set_attribute(KeyValue::new("turn", *turn as i64));
                    if let Some(g) = gate {
                        span.set_attribute(KeyValue::new("gate", g.as_str().to_string()));
                    }
                    if let Some(preview) = output_preview {
                        span.set_attribute(KeyValue::new(
                            "langfuse.observation.output",
                            preview.clone(),
                        ));
                    } else {
                        span.set_attribute(KeyValue::new(
                            "langfuse.observation.output",
                            json!({
                                "is_error": is_error,
                                "output_bytes": output_bytes,
                                "gate": gate.as_ref().map(TraceGateDecision::as_str),
                            })
                            .to_string(),
                        ));
                    }
                    if *is_error {
                        span.set_status(Status::error("tool error"));
                    } else {
                        span.set_status(Status::Ok);
                    }
                    span.end_with_timestamp(ms_to_system_time(*ts_ms));
                }
            }
            TraceEvent::Gate {
                ts_ms,
                run_id: _,
                turn,
                call_id,
                name,
                decision,
                message,
            } => {
                // Attach as event on tool span if open, else on turn/root.
                let cx = state
                    .tools
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| Self::parent_cx(&state, Some(*turn)));
                let span = cx.span();
                let mut attrs = vec![
                    KeyValue::new("decision", decision.as_str().to_string()),
                    KeyValue::new("tool", name.clone()),
                    KeyValue::new("turn", *turn as i64),
                ];
                if let Some(m) = message {
                    attrs.push(KeyValue::new("message", m.clone()));
                }
                span.add_event_with_timestamp(
                    format!("gate:{}", decision.as_str()),
                    ms_to_system_time(*ts_ms),
                    attrs,
                );
            }
            TraceEvent::Compaction {
                ts_ms,
                run_id: _,
                before_tokens,
                after_tokens,
                mode,
                duration_ms,
            } => {
                let parent = Self::parent_cx(&state, None);
                let start = duration_ms
                    .map(|d| ts_ms.saturating_sub(d))
                    .unwrap_or(*ts_ms);
                let mut attrs = vec![KeyValue::new("langfuse.observation.type", "span")];
                attrs = Self::with_propagated(&state, attrs);
                let mut span = self
                    .tracer
                    .span_builder("compaction")
                    .with_kind(SpanKind::Internal)
                    .with_start_time(ms_to_system_time(start))
                    .with_attributes(attrs)
                    .start_with_context(&self.tracer, &parent);
                if let Some(b) = before_tokens {
                    span.set_attribute(KeyValue::new("before_tokens", *b as i64));
                }
                if let Some(a) = after_tokens {
                    span.set_attribute(KeyValue::new("after_tokens", *a as i64));
                }
                if let Some(m) = mode {
                    span.set_attribute(KeyValue::new("mode", m.clone()));
                }
                if let Some(d) = duration_ms {
                    span.set_attribute(KeyValue::new("duration_ms", *d as i64));
                }
                span.end_with_timestamp(ms_to_system_time(*ts_ms));
            }
            TraceEvent::Score {
                ts_ms: _,
                run_id,
                task_id,
                pass,
                score,
                checks,
                notes,
            } => {
                // Capture ids before dropping the state lock for flush/score.
                let lf_trace_id = state
                    .otel_trace_id
                    .clone()
                    .unwrap_or_else(|| run_id.clone());
                let comment = {
                    let mut parts = Vec::new();
                    if let Some(t) = task_id {
                        parts.push(format!("task={t}"));
                    }
                    parts.push(format!("pass={pass}"));
                    parts.push(format!("run_id={run_id}"));
                    if let Some(n) = notes {
                        parts.push(n.clone());
                    }
                    let failed: Vec<_> = checks
                        .iter()
                        .filter(|c| !c.pass)
                        .map(|c| c.name.as_str())
                        .collect();
                    if !failed.is_empty() {
                        parts.push(format!("failed={}", failed.join(",")));
                    }
                    parts.join("; ")
                };
                let task = task_id.clone();
                let pass = *pass;
                let score = *score;
                drop(state);

                // Flush spans first so the trace exists before score linking.
                if self.exporting {
                    self.flush();
                }

                self.enqueue_score(
                    &lf_trace_id,
                    "harness_pass",
                    json!(if pass { 1.0 } else { 0.0 }),
                    "BOOLEAN",
                    Some(&comment),
                    task.as_deref(),
                );
                self.enqueue_score(
                    &lf_trace_id,
                    "harness_score",
                    json!(score),
                    "NUMERIC",
                    None,
                    task.as_deref(),
                );
                // Re-lock not needed — Score is the last event for a run in bench.
                return;
            }
        }
    }

    fn enqueue_score(
        &self,
        trace_id: &str,
        name: &str,
        value: serde_json::Value,
        data_type: &str,
        comment: Option<&str>,
        task_id: Option<&str>,
    ) {
        if !self.exporting {
            // Memory-only fallback: skip network.
            return;
        }
        let mut body = json!({
            "traceId": trace_id,
            "name": name,
            "value": value,
            "dataType": data_type,
        });
        if let Some(c) = comment {
            body["comment"] = json!(c);
        }
        if let Some(t) = task_id {
            body["metadata"] = json!({ "task_id": t });
        }
        if let Some(env) = tracing_environment() {
            body["environment"] = json!(env);
        }
        // Blocking HTTP off the async runtime thread; joined in shutdown().
        let client = self.http.clone();
        let url = self.config.scores_url();
        let auth = self.config.basic_auth_header();
        let name_owned = name.to_string();
        let trace_owned = trace_id.to_string();
        match std::thread::Builder::new()
            .name("langfuse-score".into())
            .spawn(move || {
                match client
                    .post(url)
                    .header("Authorization", auth)
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::debug!(name = %name_owned, trace_id = %trace_owned, "langfuse: score ok");
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().unwrap_or_default();
                        tracing::warn!(
                            %status,
                            body = %truncate(&text, 200),
                            name = %name_owned,
                            "langfuse: score post failed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, name = %name_owned, "langfuse: score request error")
                    }
                }
            }) {
            Ok(handle) => {
                if let Ok(mut pending) = self.pending_scores.lock() {
                    pending.push(handle);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "langfuse: failed to spawn score worker");
            }
        }
    }
}

impl TraceSink for LangfuseTraceSink {
    fn record(&self, event: TraceEvent) {
        if let Ok(mut v) = self.events.lock() {
            v.push(event.clone());
        }
        self.apply_event(&event);
    }
}

impl Drop for LangfuseTraceSink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn format_iso_ms(ms: u64) -> String {
    // Minimal UTC ISO-8601 without pulling chrono into hot path if unused elsewhere.
    // Prefer chrono if available in crate.
    use chrono::{TimeZone, Utc};
    match Utc.timestamp_millis_opt(ms as i64) {
        chrono::LocalResult::Single(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        _ => "1970-01-01T00:00:00.000Z".into(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_otlp_url() {
        let cfg = LangfuseConfig {
            public_key: "pk".into(),
            secret_key: "sk".into(),
            base_url: "https://us.cloud.langfuse.com".into(),
        };
        assert_eq!(
            cfg.otlp_endpoint(),
            "https://us.cloud.langfuse.com/api/public/otel/v1/traces"
        );
        assert_eq!(cfg.otlp_traces_url(), cfg.otlp_endpoint());
        assert!(cfg.basic_auth_header().starts_with("Basic "));
    }

    #[test]
    fn basic_auth_encodes_pk_sk() {
        use base64::Engine;
        let cfg = LangfuseConfig {
            public_key: "pk-lf-x".into(),
            secret_key: "sk-lf-y".into(),
            base_url: "https://cloud.langfuse.com".into(),
        };
        let h = cfg.basic_auth_header();
        let b64 = h.trim_start_matches("Basic ");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), "pk-lf-x:sk-lf-y");
    }

    #[test]
    fn tags_attr_is_array() {
        let kv = tags_attr(&["one".into(), "agent".into(), "task:x".into()]);
        match kv.value {
            Value::Array(Array::String(v)) => {
                assert_eq!(v.len(), 3);
            }
            other => panic!("expected string array, got {other:?}"),
        }
    }

    #[test]
    fn memory_sink_records_events() {
        let sink = LangfuseTraceSink::memory_only(LangfuseConfig {
            public_key: "pk".into(),
            secret_key: "sk".into(),
            base_url: "http://localhost".into(),
        });
        sink.record(TraceEvent::RunStart {
            ts_ms: 1,
            run_id: "run_test".into(),
            agent: "one".into(),
            agent_version: Some("0.1.0".into()),
            provider: Some("mock".into()),
            model: Some("m".into()),
            task_id: Some("t1".into()),
            config: Some(json!({"max_turns": 4})),
            session_id: Some("sess-abc".into()),
            user_id: Some("u1".into()),
            trace_full: false,
        });
        sink.record(TraceEvent::LlmRequest {
            ts_ms: 2,
            run_id: "run_test".into(),
            turn: 0,
            message_count: 1,
            tools_n: 0,
            system_prompt_len: 10,
            input_preview: None,
        });
        sink.record(TraceEvent::LlmResponse {
            ts_ms: 50,
            run_id: "run_test".into(),
            turn: 0,
            latency_ms: 48,
            ttft_ms: Some(5),
            stop_reason: "end".into(),
            tool_calls_n: 0,
            text_len: 3,
            thinking_len: 0,
            usage: one_core::TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            provider: "mock".into(),
            model: "m".into(),
            output_preview: Some("hi".into()),
        });
        sink.record(TraceEvent::RunEnd {
            ts_ms: 60,
            run_id: "run_test".into(),
            status: TraceRunStatus::Ok,
            turns: 1,
            wall_ms: 59,
            usage: one_core::TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            final_text_len: Some(3),
            final_text_preview: Some("hi".into()),
            error: None,
        });
        assert_eq!(sink.events().len(), 4);
        sink.shutdown();
    }

    #[test]
    fn propagated_includes_session_and_tags() {
        let attrs = build_propagated(
            Some("sess-1"),
            Some("user-a"),
            Some("kit-fix"),
            Some("0.1.0"),
            Some("mock"),
            Some("m"),
            Some(&json!({"suite": "smoke"})),
            "one:kit-fix",
        );
        let keys: Vec<_> = attrs.iter().map(|a| a.key.as_str()).collect();
        assert!(keys.contains(&"langfuse.session.id"));
        assert!(keys.contains(&"langfuse.user.id"));
        assert!(keys.contains(&"langfuse.trace.tags"));
        assert!(keys.contains(&"langfuse.trace.metadata.suite"));
        assert!(keys.contains(&"langfuse.trace.metadata.task_id"));
    }
}
