//! Connect MCP servers and expose their tools.
//!
//! **Async load (Grok-style):**
//! - Config is read synchronously (disk only).
//! - Connections run in a background task (`buffer_unordered(8)`).
//! - Session / TUI start is not blocked on cold `npx` downloads.
//! - Each finished server bumps a generation counter; the host re-syncs
//!   tools onto the Agent before the next prompt.
//! - `/new` keeps the live connection pool (shared across conversations).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use one_core::tool::Tool;
use parking_lot::RwLock;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use tracing::{info, warn};

use crate::catalog::{
    build_prompt_announcement, param_names_from_schema, sanitize_description, search_tools,
    truncate_description, SearchSnapshot, ServerSummary, ToolExposure, ToolMeta,
};
use crate::config::{
    import_servers_to_user, load_effective, scan_import_candidates, set_server_disabled_persistent,
    ConfigSourceKind, ImportCandidate, ImportReport, LoadedMcpConfig, McpConfig, McpServerConfig,
    DEFAULT_MAX_OUTPUT_BYTES,
};
use crate::error::{McpError, Result};
use crate::meta_tools;
use crate::tool::tools_from_list;

/// Health snapshot for `one mcp doctor`.
#[derive(Debug, Clone)]
pub struct ServerHealth {
    pub name: String,
    pub transport: String,
    pub ok: bool,
    pub message: String,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLoadStatus {
    /// No servers configured or MCP disabled.
    Idle,
    /// Background handshakes still running.
    Loading,
    /// All configured servers settled (ok or failed).
    Ready,
}

/// UI row for MCP manager panel.
#[derive(Debug, Clone)]
pub struct McpServerRow {
    pub name: String,
    pub source: String,
    pub transport: String,
    /// ready | loading | failed | disabled | idle
    pub status: String,
    pub enabled: bool,
    pub tool_count: usize,
    pub detail: String,
}

/// Compact status-bar / prompt-meta chip (live-readable via shared state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpChipKind {
    /// Still connecting some servers.
    Loading,
    /// All enabled servers ready.
    Ok,
    /// Some ready, some failed (or partial).
    Partial,
    /// All enabled failed / none up.
    Error,
}

/// e.g. text=`MCP 4/5`, kind drives color.
#[derive(Debug, Clone)]
pub struct McpChip {
    pub text: String,
    pub kind: McpChipKind,
    pub ready: usize,
    pub total: usize,
}

/// A compact, model-facing snapshot of one configured MCP server.
///
/// This deliberately contains status and tool names, but never tool schemas or
/// connection secrets. It is intended for the `mcp_status` meta-tool and for
/// Grok-style runtime reminders.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    /// `ready`, `connecting`, `unavailable`, `auth_required`, or `disabled`.
    pub status: &'static str,
    pub enabled: bool,
    pub transport: &'static str,
    pub source: Option<String>,
    pub tool_count: usize,
    pub tools: Vec<String>,
    /// Server-level instructions or title from the MCP initialize response.
    /// This is not a per-tool description; detailed tool descriptions and
    /// schemas remain deferred to `search_tool`.
    pub description: Option<String>,
    pub detail: Option<String>,
}

/// Runtime MCP status, distinct from the static MCP configuration.
#[derive(Debug, Clone)]
pub struct McpStatusSnapshot {
    /// `ready` means the catalog has settled; individual servers may still be
    /// unavailable. `connecting` means at least one enabled server is pending.
    pub status: &'static str,
    pub configured: usize,
    pub enabled: usize,
    pub ready: usize,
    pub connecting: usize,
    pub unavailable: usize,
    pub auth_required: usize,
    pub disabled: usize,
    pub tool_count: usize,
    pub servers: Vec<McpServerStatus>,
}

/// Tracks MCP server-status reminder state for one conversation.
#[derive(Debug, Clone, Default)]
pub struct McpReminderState {
    last: Option<McpStatusFingerprint>,
}

/// Kind of MCP reminder to inject before the next model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpReminderKind {
    /// Full snapshot, typically at the beginning of a conversation.
    Full,
    /// Only changed server rows since the previous reminder.
    Delta,
}

/// A model-visible reminder that should be injected as a `<system-reminder>`
/// user notice, not appended to the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpReminder {
    pub kind: McpReminderKind,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpStatusFingerprint {
    status: &'static str,
    servers: BTreeMap<String, McpServerFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpServerFingerprint {
    status: &'static str,
    enabled: bool,
    transport: &'static str,
    tool_count: usize,
    detail: Option<String>,
}

impl McpReminderState {
    /// Build the next model-visible MCP reminder, if the runtime status changed.
    ///
    /// The first non-empty snapshot is emitted as a full reminder. Later changes
    /// are emitted as compact deltas. The caller should inject the returned body
    /// as a `<system-reminder>` user notice so the cached system prompt stays
    /// stable while MCP's live state evolves.
    pub fn next(&mut self, snapshot: &McpStatusSnapshot) -> Option<McpReminder> {
        if snapshot.configured == 0 || snapshot.servers.is_empty() {
            self.last = None;
            return None;
        }

        let current = McpStatusFingerprint::from_snapshot(snapshot);
        let reminder = match &self.last {
            None => {
                let body = render_full_mcp_reminder(snapshot);
                (!body.trim().is_empty()).then_some(McpReminder {
                    kind: McpReminderKind::Full,
                    body,
                })
            }
            Some(previous) if previous != &current => {
                let body = render_delta_mcp_reminder(snapshot, previous, &current);
                (!body.trim().is_empty()).then_some(McpReminder {
                    kind: McpReminderKind::Delta,
                    body,
                })
            }
            Some(_) => None,
        };
        self.last = Some(current);
        reminder
    }

    /// Force the next non-empty status snapshot to render as a full reminder.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

impl McpStatusFingerprint {
    fn from_snapshot(snapshot: &McpStatusSnapshot) -> Self {
        let servers = snapshot
            .servers
            .iter()
            .map(|server| {
                (
                    server.name.clone(),
                    McpServerFingerprint {
                        status: server.status,
                        enabled: server.enabled,
                        transport: server.transport,
                        tool_count: server.tool_count,
                        detail: server.detail.clone(),
                    },
                )
            })
            .collect();
        Self {
            status: snapshot.status,
            servers,
        }
    }
}

fn render_full_mcp_reminder(snapshot: &McpStatusSnapshot) -> String {
    let mut out = String::new();
    if snapshot.ready > 0 {
        out.push_str("Connected MCP servers:\n");
        for server in snapshot
            .servers
            .iter()
            .filter(|server| server.status == "ready")
        {
            out.push_str(&format_mcp_server_summary_line(server));
        }
    }

    if snapshot.connecting > 0 {
        if !out.is_empty() {
            out.push('\n');
        }
        append_connecting_servers(&mut out, snapshot);
    }
    append_unavailable_servers(&mut out, snapshot);
    if out.trim().is_empty() {
        return String::new();
    }
    out.push_str(mcp_runtime_usage_hint());
    out
}

fn render_delta_mcp_reminder(
    snapshot: &McpStatusSnapshot,
    previous: &McpStatusFingerprint,
    current: &McpStatusFingerprint,
) -> String {
    let mut out = String::new();
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut removed = Vec::new();

    let names: BTreeSet<String> = previous
        .servers
        .keys()
        .chain(current.servers.keys())
        .cloned()
        .collect();
    for name in names {
        match (previous.servers.get(&name), current.servers.get(&name)) {
            (None, Some(next)) => {
                if next.status == "ready" {
                    added.push(name);
                }
            }
            (Some(_), None) => removed.push(name),
            (Some(prev), Some(next)) if prev != next => {
                if prev.status != "ready" && next.status == "ready" {
                    added.push(name);
                } else if next.status == "ready" {
                    updated.push(name);
                }
            }
            _ => {}
        }
    }

    if !added.is_empty() {
        out.push_str("MCP server(s) connected:\n");
        for name in &added {
            if let Some(server) = snapshot.servers.iter().find(|server| &server.name == name) {
                out.push_str(&format_mcp_server_summary_line(server));
            }
        }
    }
    if !updated.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("MCP server(s) updated:\n");
        for name in &updated {
            if let Some(server) = snapshot.servers.iter().find(|server| &server.name == name) {
                out.push_str(&format_mcp_server_summary_line(server));
            }
        }
    }
    if !removed.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "MCP server(s) disconnected: {}\n",
            removed.join(", ")
        ));
    }

    if snapshot.connecting > 0 {
        if !out.is_empty() {
            out.push('\n');
        }
        append_connecting_servers(&mut out, snapshot);
    }
    append_unavailable_servers(&mut out, snapshot);

    if out.trim().is_empty() {
        return String::new();
    }
    out.push_str(mcp_runtime_usage_hint());
    out
}

fn append_connecting_servers(out: &mut String, snapshot: &McpStatusSnapshot) {
    out.push_str("MCP servers currently connecting (tools will become available shortly):\n");
    for server in snapshot
        .servers
        .iter()
        .filter(|server| server.status == "connecting")
    {
        out.push_str(&format!("- {}\n", server.name));
    }
    out.push_str(
        "\nDo not attempt to use tools from these servers yet. If the user's request likely requires one of these servers, mention that the server is still connecting and proceed with what you can do in the meantime.\n",
    );
}

fn append_unavailable_servers(out: &mut String, snapshot: &McpStatusSnapshot) {
    let unavailable: Vec<_> = snapshot
        .servers
        .iter()
        .filter(|server| matches!(server.status, "unavailable" | "auth_required" | "disabled"))
        .collect();
    if unavailable.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("MCP servers that are not currently usable:\n");
    for server in unavailable {
        let detail = server
            .detail
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        out.push_str(&format!("- {}: {}{}\n", server.name, server.status, detail));
    }
}

fn format_mcp_server_summary_line(server: &McpServerStatus) -> String {
    let tool_word = if server.tool_count == 1 {
        "tool"
    } else {
        "tools"
    };
    match server
        .description
        .as_deref()
        .map(sanitize_description)
        .map(|s| truncate_description(&s))
        .filter(|s| !s.is_empty())
    {
        Some(description) => format!(
            "- {} ({} {}): {}\n",
            server.name, server.tool_count, tool_word, description
        ),
        None => format!("- {} ({} {})\n", server.name, server.tool_count, tool_word),
    }
}

fn mcp_runtime_usage_hint() -> &'static str {
    "\nTo use MCP tools, you MUST call `search_tool` first to retrieve the tool's input schema before calling `use_tool`. NEVER guess parameter names — always use the exact schema returned by `search_tool`.\nMCP tool schemas and per-tool descriptions are not preloaded into this reminder; use `search_tool` for details and call MCP tools only through `use_tool` with the qualified `server__tool` name.\n"
}

#[cfg(test)]
mod reminder_tests {
    use super::*;

    fn server(
        name: &str,
        status: &'static str,
        tool_count: usize,
        tools: &[&str],
        description: Option<&str>,
        detail: Option<&str>,
    ) -> McpServerStatus {
        McpServerStatus {
            name: name.into(),
            status,
            enabled: status != "disabled",
            transport: "stdio",
            source: Some("test".into()),
            tool_count,
            tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
            description: description.map(str::to_string),
            detail: detail.map(str::to_string),
        }
    }

    fn snapshot(servers: Vec<McpServerStatus>) -> McpStatusSnapshot {
        let configured = servers.len();
        let enabled = servers.iter().filter(|server| server.enabled).count();
        let ready = servers
            .iter()
            .filter(|server| server.status == "ready")
            .count();
        let connecting = servers
            .iter()
            .filter(|server| server.status == "connecting")
            .count();
        let unavailable = servers
            .iter()
            .filter(|server| server.status == "unavailable")
            .count();
        let auth_required = servers
            .iter()
            .filter(|server| server.status == "auth_required")
            .count();
        let disabled = servers
            .iter()
            .filter(|server| server.status == "disabled")
            .count();
        let tool_count = servers.iter().map(|server| server.tool_count).sum();
        McpStatusSnapshot {
            status: if connecting > 0 {
                "connecting"
            } else {
                "ready"
            },
            configured,
            enabled,
            ready,
            connecting,
            unavailable,
            auth_required,
            disabled,
            tool_count,
            servers,
        }
    }

    #[test]
    fn full_reminder_lists_servers_not_tool_names() {
        let snap = snapshot(vec![server(
            "deepwiki",
            "ready",
            3,
            &["ask_question", "read_wiki_contents", "read_wiki_structure"],
            Some("DeepWiki MCP provides AI-powered documentation."),
            None,
        )]);
        let text = render_full_mcp_reminder(&snap);
        assert!(text.contains("Connected MCP servers:"), "{text}");
        assert!(
            text.contains("- deepwiki (3 tools): DeepWiki MCP provides"),
            "{text}"
        );
        assert!(
            !text.contains("ask_question"),
            "per-tool names stay out: {text}"
        );
        assert!(text.contains("search_tool"), "{text}");
        assert!(text.contains("use_tool"), "{text}");
    }

    #[test]
    fn full_reminder_mentions_connecting_servers() {
        let snap = snapshot(vec![server(
            "context-mode",
            "connecting",
            0,
            &[],
            None,
            Some("startup handshake is still in progress"),
        )]);
        let text = render_full_mcp_reminder(&snap);
        assert!(text.contains("currently connecting"), "{text}");
        assert!(text.contains("- context-mode"), "{text}");
        assert!(text.contains("Do not attempt to use tools"), "{text}");
    }

    #[test]
    fn state_emits_full_then_delta() {
        let mut state = McpReminderState::default();
        let loading = snapshot(vec![server("docs", "connecting", 0, &[], None, None)]);
        let first = state.next(&loading).expect("full reminder");
        assert_eq!(first.kind, McpReminderKind::Full);

        let ready = snapshot(vec![server(
            "docs",
            "ready",
            2,
            &["search", "read"],
            Some("Docs server"),
            None,
        )]);
        let second = state.next(&ready).expect("delta reminder");
        assert_eq!(second.kind, McpReminderKind::Delta);
        assert!(
            second.body.contains("MCP server(s) connected:"),
            "{}",
            second.body
        );
        assert!(
            second.body.contains("- docs (2 tools): Docs server"),
            "{}",
            second.body
        );
    }
}

/// Cheap handle for the TUI to poll MCP progress every redraw (no CLI hop).
#[derive(Clone)]
pub struct McpProgressHandle {
    shared: Arc<SharedState>,
    process_disabled: bool,
}

impl McpProgressHandle {
    /// `None` when MCP is off for this process or nothing is configured.
    pub fn chip(&self) -> Option<McpChip> {
        if self.process_disabled {
            return None;
        }
        let config = self.shared.config.read();
        let total = config.mcp_servers.len();
        if total == 0 {
            return None;
        }
        let disabled = self.shared.disabled_names.read();
        let live = self.shared.live.read();
        let failures = self.shared.failures.read();
        let loading = self.shared.loading.load(Ordering::SeqCst)
            || self.shared.pending.load(Ordering::SeqCst) > 0;

        let ready = live.iter().filter(|s| !disabled.contains(&s.name)).count();
        let failed = failures
            .iter()
            .filter(|(n, _)| !disabled.contains(n))
            .count();
        let enabled_total = config
            .mcp_servers
            .iter()
            .filter(|(n, c)| !disabled.contains(*n) && c.enabled != Some(false))
            .count();

        // Prefer enabled-only denominator so toggling off shrinks the bar.
        let denom = if enabled_total > 0 {
            enabled_total
        } else {
            total
        };
        let ready_clamped = ready.min(denom);

        let kind = if loading && ready_clamped < denom {
            McpChipKind::Loading
        } else if failed > 0 && ready_clamped == 0 {
            McpChipKind::Error
        } else if failed > 0 || ready_clamped < denom {
            McpChipKind::Partial
        } else {
            McpChipKind::Ok
        };

        let text = if loading && ready_clamped < denom {
            format!("MCP {ready_clamped}/{denom}…")
        } else {
            format!("MCP {ready_clamped}/{denom}")
        };

        Some(McpChip {
            text,
            kind,
            ready: ready_clamped,
            total: denom,
        })
    }
}

struct LiveServer {
    name: String,
    /// Optional description from MCP initialize (`instructions` / server title).
    description: Option<String>,
    _service: RunningService<RoleClient, ()>,
    tools: Vec<Arc<dyn Tool>>,
    transport: String,
}

struct SharedState {
    config: RwLock<McpConfig>,
    server_sources: RwLock<std::collections::BTreeMap<String, ConfigSourceKind>>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    failures: RwLock<Vec<(String, String)>>,
    live: RwLock<Vec<LiveServer>>,
    /// User-disabled names (persisted).
    disabled_names: RwLock<HashSet<String>>,
    /// Bumped when the tool set changes (host polls this).
    generation: AtomicU64,
    loading: AtomicBool,
    pending: AtomicU64,
}

/// Shared handle for deferred meta tools (`search_tool` / `use_tool`).
///
/// Clones are cheap; all point at the process-level MCP connection pool.
#[derive(Clone)]
pub struct McpCatalog {
    shared: Arc<SharedState>,
    process_disabled: bool,
}

impl McpCatalog {
    /// Search connected MCP tools (exact name fast path, then BM25).
    pub fn search(&self, query: &str, limit: usize) -> SearchSnapshot {
        let is_ready = !self.shared.loading.load(Ordering::SeqCst)
            && self.shared.pending.load(Ordering::SeqCst) == 0;
        let metas = catalog_tool_metas(&self.shared);
        search_tools(&metas, query, limit, is_ready)
    }

    /// Look up a connected tool by qualified public name.
    pub fn find_tool(&self, qualified_name: &str) -> Option<Arc<dyn Tool>> {
        self.shared
            .tools
            .read()
            .iter()
            .find(|t| t.definition().name == qualified_name)
            .cloned()
    }

    pub fn tool_count(&self) -> usize {
        self.shared.tools.read().len()
    }

    /// Snapshot of all configured MCP server states, including servers with no
    /// currently indexed tools.
    pub fn status_snapshot(&self) -> McpStatusSnapshot {
        if self.process_disabled {
            return McpStatusSnapshot {
                status: "disabled",
                configured: 0,
                enabled: 0,
                ready: 0,
                connecting: 0,
                unavailable: 0,
                auth_required: 0,
                disabled: 0,
                tool_count: 0,
                servers: Vec::new(),
            };
        }
        McpManager {
            shared: Arc::clone(&self.shared),
            _bg: None,
            disabled: false,
        }
        .status_snapshot()
    }
}

fn catalog_tool_metas(shared: &SharedState) -> Vec<ToolMeta> {
    let disabled = shared.disabled_names.read().clone();
    let live = shared.live.read();
    let mut out = Vec::new();
    for srv in live.iter() {
        if disabled.contains(&srv.name) {
            continue;
        }
        for t in &srv.tools {
            let def = t.definition();
            // public_name is server__tool; recover bare tool name after first `__`.
            let tool_name = def
                .name
                .split_once("__")
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| def.name.clone());
            out.push(ToolMeta {
                qualified_name: def.name.clone(),
                server_name: srv.name.clone(),
                tool_name,
                description: def.description.clone(),
                parameters: param_names_from_schema(&def.parameters),
                input_schema: def.parameters.clone(),
            });
        }
    }
    out
}

fn server_summaries_from(shared: &SharedState) -> Vec<ServerSummary> {
    let disabled = shared.disabled_names.read().clone();
    let live = shared.live.read();
    let mut out: Vec<ServerSummary> = live
        .iter()
        .filter(|s| !disabled.contains(&s.name))
        .map(|s| {
            let mut tool_names: Vec<String> = s
                .tools
                .iter()
                .map(|t| {
                    let n = t.definition().name;
                    n.split_once("__")
                        .map(|(_, rest)| rest.to_string())
                        .unwrap_or(n)
                })
                .collect();
            tool_names.sort();
            ServerSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                tool_count: s.tools.len(),
                tool_names,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Process-level MCP runtime (held by AppRuntime for the whole process).
///
/// Connections are **shared across `/new` sessions** — only messages clear.
pub struct McpManager {
    shared: Arc<SharedState>,
    /// Keeps the background connect task alive.
    _bg: Option<tokio::task::JoinHandle<()>>,
    disabled: bool,
}

impl McpManager {
    pub fn empty() -> Self {
        Self {
            shared: Arc::new(SharedState {
                config: RwLock::new(McpConfig::empty()),
                server_sources: RwLock::new(Default::default()),
                tools: RwLock::new(Vec::new()),
                failures: RwLock::new(Vec::new()),
                live: RwLock::new(Vec::new()),
                disabled_names: RwLock::new(HashSet::new()),
                generation: AtomicU64::new(0),
                loading: AtomicBool::new(false),
                pending: AtomicU64::new(0),
            }),
            _bg: None,
            disabled: true,
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Live progress for the status bar (safe to poll every frame).
    pub fn progress_handle(&self) -> McpProgressHandle {
        McpProgressHandle {
            shared: Arc::clone(&self.shared),
            process_disabled: self.disabled,
        }
    }

    pub fn config(&self) -> McpConfig {
        self.shared.config.read().clone()
    }

    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> McpLoadStatus {
        if self.disabled {
            return McpLoadStatus::Idle;
        }
        if self.shared.loading.load(Ordering::SeqCst)
            || self.shared.pending.load(Ordering::SeqCst) > 0
        {
            return McpLoadStatus::Loading;
        }
        if self.shared.config.read().mcp_servers.is_empty() {
            return McpLoadStatus::Idle;
        }
        McpLoadStatus::Ready
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.status(), McpLoadStatus::Loading)
    }

    pub fn failures(&self) -> Vec<(String, String)> {
        self.shared.failures.read().clone()
    }

    /// Snapshot of currently connected tools (safe to call from async without await).
    ///
    /// Full MCP tool set (for dispatch / doctor). For **model-visible** tools, use
    /// [`Self::model_visible_tools`] which respects deferred exposure.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.shared.tools.read().clone()
    }

    pub fn tool_count(&self) -> usize {
        self.shared.tools.read().len()
    }

    /// Return a compact snapshot of the live MCP runtime.
    ///
    /// Unlike `search_tool`, this reports configured servers even when they
    /// have no tools because they are still connecting, disabled, or failed.
    /// The snapshot is computed from shared runtime state, not from the
    /// model's conversation and not from a tool-call failure.
    pub fn status_snapshot(&self) -> McpStatusSnapshot {
        if self.disabled {
            return McpStatusSnapshot {
                status: "disabled",
                configured: 0,
                enabled: 0,
                ready: 0,
                connecting: 0,
                unavailable: 0,
                auth_required: 0,
                disabled: 0,
                tool_count: 0,
                servers: Vec::new(),
            };
        }

        let config = self.shared.config.read().clone();
        let disabled_names = self.shared.disabled_names.read().clone();
        let live = self.shared.live.read();
        let failures = self.shared.failures.read().clone();
        let loading = self.is_loading();
        let mut servers = Vec::with_capacity(config.mcp_servers.len());

        for (name, cfg) in &config.mcp_servers {
            let disabled = disabled_names.contains(name) || cfg.enabled == Some(false);
            let transport = if cfg.is_http() { "http" } else { "stdio" };
            let source = self
                .server_source(name)
                .map(|kind| kind.as_str().to_string());
            let live_server = live.iter().find(|server| server.name == *name);
            let failure = failures.iter().find(|(server, _)| server == name);

            let server = if disabled {
                McpServerStatus {
                    name: name.clone(),
                    status: "disabled",
                    enabled: false,
                    transport,
                    source,
                    tool_count: 0,
                    tools: Vec::new(),
                    description: None,
                    detail: Some("disabled by configuration".into()),
                }
            } else if let Some(live_server) = live_server {
                let mut tools: Vec<String> = live_server
                    .tools
                    .iter()
                    .map(|tool| {
                        tool.definition()
                            .name
                            .split_once("__")
                            .map(|(_, bare)| bare.to_string())
                            .unwrap_or_else(|| tool.definition().name.clone())
                    })
                    .collect();
                tools.sort();
                McpServerStatus {
                    name: name.clone(),
                    status: "ready",
                    enabled: true,
                    transport,
                    source,
                    tool_count: tools.len(),
                    tools,
                    description: live_server.description.clone(),
                    detail: None,
                }
            } else if let Some((_, message)) = failure {
                let lower = message.to_ascii_lowercase();
                let auth = lower.contains("oauth")
                    || lower.contains("auth")
                    || lower.contains("login")
                    || lower.contains("unauthorized")
                    || lower.contains("401");
                McpServerStatus {
                    name: name.clone(),
                    status: if auth { "auth_required" } else { "unavailable" },
                    enabled: true,
                    transport,
                    source,
                    tool_count: 0,
                    tools: Vec::new(),
                    description: None,
                    detail: Some(message.clone()),
                }
            } else {
                McpServerStatus {
                    name: name.clone(),
                    status: if loading { "connecting" } else { "unavailable" },
                    enabled: true,
                    transport,
                    source,
                    tool_count: 0,
                    tools: Vec::new(),
                    description: None,
                    detail: if loading {
                        Some("startup handshake is still in progress".into())
                    } else {
                        Some("no live connection".into())
                    },
                }
            };
            servers.push(server);
        }

        let configured = servers.len();
        let enabled = servers.iter().filter(|server| server.enabled).count();
        let ready = servers
            .iter()
            .filter(|server| server.status == "ready")
            .count();
        let connecting = servers
            .iter()
            .filter(|server| server.status == "connecting")
            .count();
        let unavailable = servers
            .iter()
            .filter(|server| server.status == "unavailable")
            .count();
        let auth_required = servers
            .iter()
            .filter(|server| server.status == "auth_required")
            .count();
        let disabled = servers
            .iter()
            .filter(|server| server.status == "disabled")
            .count();
        let status = if connecting > 0 {
            "connecting"
        } else {
            "ready"
        };
        let tool_count = servers.iter().map(|server| server.tool_count).sum();

        McpStatusSnapshot {
            status,
            configured,
            enabled,
            ready,
            connecting,
            unavailable,
            auth_required,
            disabled,
            tool_count,
            servers,
        }
    }

    /// Effective tool exposure (config + env).
    pub fn tool_exposure(&self) -> ToolExposure {
        if self.disabled {
            return ToolExposure::Deferred;
        }
        self.shared.config.read().effective_tool_exposure()
    }

    /// Catalog handle for meta tools / search.
    pub fn catalog(&self) -> McpCatalog {
        McpCatalog {
            shared: Arc::clone(&self.shared),
            process_disabled: self.disabled,
        }
    }

    /// Tools to register on the agent for the model to call.
    ///
    /// - **Deferred** (default): `search_tool` + `use_tool` only (when any server configured).
    /// - **Direct**: every connected MCP tool schema.
    pub fn model_visible_tools(&self) -> Vec<Arc<dyn Tool>> {
        if self.disabled {
            return Vec::new();
        }
        match self.tool_exposure() {
            ToolExposure::Direct => self.tools(),
            ToolExposure::Deferred => {
                let has_configured = !self.shared.config.read().mcp_servers.is_empty();
                if !has_configured {
                    return Vec::new();
                }
                meta_tools::meta_tools(self.catalog())
            }
        }
    }

    /// Connected server summaries for announcements / UI.
    pub fn server_summaries(&self) -> Vec<ServerSummary> {
        if self.disabled {
            return Vec::new();
        }
        server_summaries_from(&self.shared)
    }

    /// System-prompt section for deferred mode (server list + usage hint).
    ///
    /// `None` in direct mode or when MCP is off / nothing configured.
    pub fn prompt_announcement(&self) -> Option<String> {
        if self.disabled {
            return None;
        }
        if self.tool_exposure() != ToolExposure::Deferred {
            return None;
        }
        let has_configured = !self.shared.config.read().mcp_servers.is_empty();
        build_prompt_announcement(&self.server_summaries(), self.is_loading(), has_configured)
    }

    /// Search connected tools (tests / diagnostics).
    pub fn search(&self, query: &str, limit: usize) -> SearchSnapshot {
        self.catalog().search(query, limit)
    }

    pub fn server_names(&self) -> Vec<String> {
        self.shared
            .live
            .read()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    pub fn server_source(&self, name: &str) -> Option<ConfigSourceKind> {
        self.shared.server_sources.read().get(name).copied()
    }

    /// Blocking full connect (tests / `one mcp doctor`). Prefer [`Self::spawn`].
    pub async fn start(cwd: &Path) -> Result<Self> {
        let loaded = load_effective(cwd)?;
        Self::start_with_loaded(loaded, false).await
    }

    /// **Non-blocking** start: returns immediately, connects in background.
    ///
    /// Use this from `AppRuntime::build` so interactive / print modes are not
    /// delayed by cold MCP server startups.
    pub fn spawn(cwd: impl Into<PathBuf>) -> Result<Self> {
        let cwd = cwd.into();
        let loaded = load_effective(&cwd)?;
        Self::spawn_with_loaded(loaded)
    }

    pub fn spawn_with_loaded(loaded: LoadedMcpConfig) -> Result<Self> {
        let disabled_set: HashSet<String> =
            loaded.config.disabled_servers.iter().cloned().collect();
        let n_enabled = loaded.config.enabled_servers().count();
        let shared = Arc::new(SharedState {
            config: RwLock::new(loaded.config.clone()),
            server_sources: RwLock::new(loaded.server_sources.clone()),
            tools: RwLock::new(Vec::new()),
            failures: RwLock::new(Vec::new()),
            live: RwLock::new(Vec::new()),
            disabled_names: RwLock::new(disabled_set),
            generation: AtomicU64::new(0),
            loading: AtomicBool::new(n_enabled > 0),
            pending: AtomicU64::new(n_enabled as u64),
        });

        // Log sources once (sync, cheap)
        for s in &loaded.sources {
            if !s.server_names.is_empty() {
                info!(
                    source = s.kind.as_str(),
                    path = %s.path.display(),
                    servers = ?s.server_names,
                    "MCP config source"
                );
            }
        }

        if n_enabled == 0 {
            info!("MCP: no enabled servers");
            return Ok(Self {
                shared,
                _bg: None,
                disabled: false,
            });
        }

        let bg_shared = Arc::clone(&shared);
        let handle = tokio::spawn(async move {
            connect_all_background(bg_shared).await;
        });

        Ok(Self {
            shared,
            _bg: Some(handle),
            disabled: false,
        })
    }

    /// Rows for the MCP manager TUI (status + enable flag).
    ///
    /// UI-facing text stays **coarse** — no transport/source/URLs/error dumps.
    pub fn server_rows(&self) -> Vec<McpServerRow> {
        if self.disabled {
            return Vec::new();
        }
        let disabled = self.shared.disabled_names.read().clone();
        let live = self.shared.live.read();
        let failures = self.shared.failures.read();
        let loading = self.is_loading();

        let config = self.shared.config.read();
        let mut rows = Vec::new();
        for (name, cfg) in &config.mcp_servers {
            let is_disabled = disabled.contains(name) || cfg.enabled == Some(false);
            let source = self
                .server_source(name)
                .map(|k| k.as_str().to_string())
                .unwrap_or_else(|| "?".into());
            let transport = if cfg.is_http() {
                "http".into()
            } else {
                "stdio".into()
            };

            let live_srv = live.iter().find(|s| &s.name == name);
            let fail = failures.iter().find(|(n, _)| n == name);

            // Coarse status only — connection details stay in logs / `one mcp doctor`.
            let (status, detail, tool_count, enabled) = if is_disabled {
                ("off".into(), "turned off".into(), 0usize, false)
            } else if let Some(l) = live_srv {
                let n = l.tools.len();
                (
                    "ok".into(),
                    if n == 0 {
                        "connected".into()
                    } else if n == 1 {
                        "1 tool".into()
                    } else {
                        format!("{n} tools")
                    },
                    n,
                    true,
                )
            } else if fail.is_some() {
                ("error".into(), "unavailable".into(), 0, true)
            } else if loading {
                ("…".into(), "starting".into(), 0, true)
            } else {
                ("…".into(), "idle".into(), 0, true)
            };

            rows.push(McpServerRow {
                name: name.clone(),
                source,
                transport,
                status,
                enabled,
                tool_count,
                detail,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Summary for Settings root row — high level only.
    pub fn settings_summary(&self) -> String {
        if self.disabled {
            return "off".into();
        }
        let rows = self.server_rows();
        if rows.is_empty() {
            return "none".into();
        }
        let ok = rows
            .iter()
            .filter(|r| r.enabled && r.status == "ok")
            .count();
        let off = rows.iter().filter(|r| !r.enabled).count();
        let err = rows.iter().filter(|r| r.status == "error").count();
        let starting = rows
            .iter()
            .filter(|r| r.enabled && (r.status == "…" || r.status == "loading"))
            .count();
        let total = rows.len();
        // Prefer a short phrase, e.g. "3/5 ok" or "2 ok · 1 off".
        if starting > 0 && ok == 0 && err == 0 {
            return format!("starting ({total})");
        }
        let mut parts = vec![format!("{ok}/{total} ok")];
        if err > 0 {
            parts.push(format!("{err} error"));
        }
        if off > 0 {
            parts.push(format!("{off} off"));
        }
        parts.join(" · ")
    }

    /// Merge extra servers (e.g. plugin.json `mcpServers`) that are not already configured.
    ///
    /// Existing One user/project names win. New names are connected in the background.
    pub fn merge_extra_servers(
        &self,
        servers: impl IntoIterator<Item = (String, McpServerConfig)>,
        source: ConfigSourceKind,
    ) {
        if self.disabled {
            return;
        }
        let mut to_connect = Vec::new();
        {
            let mut config = self.shared.config.write();
            let mut sources = self.shared.server_sources.write();
            for (name, cfg) in servers {
                if config.mcp_servers.contains_key(&name) {
                    continue;
                }
                let enabled = cfg.enabled.unwrap_or(true);
                config.mcp_servers.insert(name.clone(), cfg.clone());
                sources.insert(name.clone(), source);
                if enabled {
                    to_connect.push((name, cfg));
                }
            }
        }
        if to_connect.is_empty() {
            return;
        }
        // `spawn_connect` bumps pending/loading per server.
        for (name, cfg) in to_connect {
            self.spawn_connect(name, cfg);
        }
    }

    /// Parse raw JSON server objects (plugin manifest shape) and merge unknowns.
    pub fn merge_plugin_server_json(
        &self,
        servers: &std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        let mut parsed = Vec::new();
        for (name, val) in servers {
            match serde_json::from_value::<McpServerConfig>(val.clone()) {
                Ok(cfg) => parsed.push((name.clone(), cfg)),
                Err(e) => {
                    warn!(server = %name, error = %e, "plugin MCP server JSON invalid; skip");
                }
            }
        }
        self.merge_extra_servers(parsed, ConfigSourceKind::Plugin);
    }

    /// Re-read One MCP config from disk and connect any new servers (for `/reload`).
    ///
    /// Existing live connections for still-configured servers are kept.
    /// Servers removed from disk stay connected until process exit (tools dropped if disabled).
    pub fn reload_from_disk(&self, cwd: &Path) -> Result<()> {
        if self.disabled {
            return Ok(());
        }
        let loaded = load_effective(cwd)?;
        let mut to_connect = Vec::new();
        {
            let mut config = self.shared.config.write();
            let mut sources = self.shared.server_sources.write();
            // Update disabled set from user config.
            {
                let mut disabled = self.shared.disabled_names.write();
                *disabled = loaded.config.disabled_servers.iter().cloned().collect();
            }
            for (name, cfg) in &loaded.config.mcp_servers {
                let is_new = !config.mcp_servers.contains_key(name);
                config.mcp_servers.insert(name.clone(), cfg.clone());
                if let Some(src) = loaded.server_sources.get(name) {
                    sources.insert(name.clone(), *src);
                }
                let disabled =
                    self.shared.disabled_names.read().contains(name) || cfg.enabled == Some(false);
                let already_live = self.shared.live.read().iter().any(|s| &s.name == name);
                if is_new && !disabled && !already_live {
                    to_connect.push((name.clone(), cfg.clone()));
                }
            }
        }
        for (name, cfg) in to_connect {
            self.spawn_connect(name, cfg);
        }
        rebuild_tools_from_live(&self.shared);
        Ok(())
    }

    /// Toggle one server on/off (persists + reconnects or drops tools).
    pub async fn set_server_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        if self.disabled {
            return Err(McpError::other(
                "MCP is disabled for this process (--no-mcp)",
            ));
        }
        if !self.shared.config.read().mcp_servers.contains_key(name) {
            return Err(McpError::other(format!("unknown MCP server `{name}`")));
        }

        set_server_disabled_persistent(name, !enabled)?;

        {
            let mut d = self.shared.disabled_names.write();
            if enabled {
                d.remove(name);
            } else {
                d.insert(name.to_string());
            }
        }

        if enabled {
            let already_live = self.shared.live.read().iter().any(|s| s.name == name);
            if already_live {
                rebuild_tools_from_live(&self.shared);
            } else {
                // Clear prior failure and connect this server in background.
                self.shared.failures.write().retain(|(n, _)| n != name);
                let cfg = self
                    .shared
                    .config
                    .read()
                    .mcp_servers
                    .get(name)
                    .cloned()
                    .ok_or_else(|| McpError::other("server vanished"))?;
                self.spawn_connect(name.to_string(), cfg);
            }
        } else {
            // Keep connection for fast re-enable, but drop tools from the agent set.
            rebuild_tools_from_live(&self.shared);
        }

        info!(server = %name, enabled, "MCP server toggle");
        Ok(())
    }

    fn spawn_connect(&self, name: String, cfg: McpServerConfig) {
        let shared = Arc::clone(&self.shared);
        self.shared.pending.fetch_add(1, Ordering::SeqCst);
        self.shared.loading.store(true, Ordering::SeqCst);
        tokio::spawn(async move {
            connect_one_into(shared, name, cfg).await;
        });
    }

    /// Scan foreign agents for import candidates (does not write).
    pub fn list_import_candidates(&self, cwd: &Path) -> Result<Vec<ImportCandidate>> {
        scan_import_candidates(cwd)
    }

    /// Import foreign MCP servers into One user config and connect them.
    ///
    /// Returns the disk import report (imported / replaced / skipped).
    pub async fn import_from_agents(
        &self,
        cwd: &Path,
        names: &[String],
        source_filter: Option<ConfigSourceKind>,
        overwrite: bool,
    ) -> Result<ImportReport> {
        if self.disabled {
            return Err(McpError::other(
                "MCP is disabled for this process (--no-mcp)",
            ));
        }

        let report = import_servers_to_user(cwd, names, source_filter, overwrite)?;
        let to_connect: Vec<String> = report
            .imported
            .iter()
            .chain(report.replaced.iter())
            .cloned()
            .collect();
        if to_connect.is_empty() {
            return Ok(report);
        }

        // Reload user+project disk view for the new entries only.
        let loaded = load_effective(cwd)?;
        for name in &to_connect {
            if let Some(cfg) = loaded.config.mcp_servers.get(name).cloned() {
                {
                    let mut config = self.shared.config.write();
                    config.mcp_servers.insert(name.clone(), cfg.clone());
                }
                self.shared
                    .server_sources
                    .write()
                    .insert(name.clone(), ConfigSourceKind::OneUser);
                // Ensure enabled
                self.shared.disabled_names.write().remove(name);
                // Drop stale live/fail so we reconnect
                self.shared.live.write().retain(|s| &s.name != name);
                self.shared.failures.write().retain(|(n, _)| n != name);
                self.spawn_connect(name.clone(), cfg);
            }
        }
        rebuild_tools_from_live(&self.shared);
        Ok(report)
    }

    pub fn is_server_enabled(&self, name: &str) -> bool {
        !self.shared.disabled_names.read().contains(name)
    }

    async fn start_with_loaded(loaded: LoadedMcpConfig, _unused: bool) -> Result<Self> {
        // Synchronous path used by tests: wait for full connect.
        let mgr = Self::spawn_with_loaded(loaded)?;
        mgr.wait_ready().await;
        Ok(mgr)
    }

    /// Wait until background loading finishes (or disabled/idle).
    pub async fn wait_ready(&self) {
        if self.disabled {
            return;
        }
        while self.is_loading() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn health(&self) -> Vec<ServerHealth> {
        let mut out = Vec::new();
        let live = self.shared.live.read();
        for s in live.iter() {
            out.push(ServerHealth {
                name: s.name.clone(),
                transport: s.transport.clone(),
                ok: true,
                message: if self.is_loading() {
                    "connected (more servers still loading)".into()
                } else {
                    "connected".into()
                },
                tool_count: s.tools.len(),
                tools: s.tools.iter().map(|t| t.definition().name).collect(),
                source: self.server_source(&s.name).map(|k| k.as_str().to_string()),
            });
        }
        let live_names: std::collections::HashSet<String> =
            live.iter().map(|s| s.name.clone()).collect();
        drop(live);

        let config = self.shared.config.read();
        for (name, msg) in self.shared.failures.read().iter() {
            out.push(ServerHealth {
                name: name.clone(),
                transport: config
                    .mcp_servers
                    .get(name)
                    .map(|c| {
                        if c.is_http() {
                            "http".into()
                        } else {
                            "stdio".into()
                        }
                    })
                    .unwrap_or_else(|| "?".into()),
                ok: false,
                message: msg.clone(),
                tool_count: 0,
                tools: vec![],
                source: self.server_source(name).map(|k| k.as_str().to_string()),
            });
        }

        // Still-pending configured servers
        if self.is_loading() {
            for (name, cfg) in config.enabled_servers() {
                if live_names.contains(name) {
                    continue;
                }
                if self.shared.failures.read().iter().any(|(n, _)| n == name) {
                    continue;
                }
                out.push(ServerHealth {
                    name: name.clone(),
                    transport: if cfg.is_http() {
                        "http".into()
                    } else {
                        "stdio".into()
                    },
                    ok: false,
                    message: "loading…".into(),
                    tool_count: 0,
                    tools: vec![],
                    source: self.server_source(name).map(|k| k.as_str().to_string()),
                });
            }
        }

        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Status line for TUI footer / notices — coarse only.
    pub fn status_line(&self) -> Option<String> {
        if self.disabled {
            return None;
        }
        match self.status() {
            McpLoadStatus::Idle => None,
            McpLoadStatus::Loading => Some("MCP starting…".into()),
            McpLoadStatus::Ready => {
                let s = self.settings_summary();
                if s == "none" {
                    None
                } else {
                    Some(format!("MCP {s}"))
                }
            }
        }
    }
}

fn rebuild_tools_from_live(shared: &SharedState) {
    let disabled = shared.disabled_names.read().clone();
    let mut tools = Vec::new();
    for live in shared.live.read().iter() {
        if disabled.contains(&live.name) {
            continue;
        }
        tools.extend(live.tools.iter().cloned());
    }
    *shared.tools.write() = tools;
    shared.generation.fetch_add(1, Ordering::SeqCst);
}

async fn connect_all_background(shared: Arc<SharedState>) {
    let jobs: Vec<(String, McpServerConfig)> = shared
        .config
        .read()
        .enabled_servers()
        .map(|(n, c)| (n.clone(), c.clone()))
        .collect();

    stream::iter(jobs)
        .map(|(name, cfg)| {
            let shared = Arc::clone(&shared);
            async move {
                connect_one_into(shared, name, cfg).await;
            }
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;

    shared.loading.store(false, Ordering::SeqCst);
    rebuild_tools_from_live(&shared);
    info!(
        tools = shared.tools.read().len(),
        servers = shared.live.read().len(),
        failures = shared.failures.read().len(),
        "MCP background load finished"
    );
}

async fn connect_one_into(shared: Arc<SharedState>, name: String, cfg: McpServerConfig) {
    // Skip if user disabled mid-flight.
    if shared.disabled_names.read().contains(&name) {
        shared.pending.fetch_sub(1, Ordering::SeqCst);
        if shared.pending.load(Ordering::SeqCst) == 0 {
            shared.loading.store(false, Ordering::SeqCst);
        }
        return;
    }

    let max_out = {
        let config = shared.config.read();
        if config.max_output_bytes == 0 {
            DEFAULT_MAX_OUTPUT_BYTES
        } else {
            config.max_output_bytes
        }
    };

    let result = connect_server(&name, &cfg, max_out).await;
    match result {
        Ok(live) => {
            info!(
                server = %name,
                tools = live.tools.len(),
                transport = %live.transport,
                "MCP server connected"
            );
            // Replace existing live entry with same name.
            {
                let mut lives = shared.live.write();
                lives.retain(|s| s.name != name);
                lives.push(live);
            }
            rebuild_tools_from_live(&shared);
        }
        Err(e) => {
            let msg = humanize_mcp_error(&e.to_string());
            warn!(server = %name, error = %msg, "MCP server failed to start");
            shared.failures.write().retain(|(n, _)| n != &name);
            shared.failures.write().push((name, msg));
            shared.generation.fetch_add(1, Ordering::SeqCst);
        }
    }
    shared.pending.fetch_sub(1, Ordering::SeqCst);
    if shared.pending.load(Ordering::SeqCst) == 0 {
        shared.loading.store(false, Ordering::SeqCst);
    }
}

/// Short, user-facing failure text for the MCP panel (not raw stack dumps).
fn humanize_mcp_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("authrequired")
        || lower.contains("www-authenticate")
        || lower.contains("oauth")
        || lower.contains("unauthorized")
        || lower.contains("401")
    {
        return "needs OAuth / login (set token or authenticate the host MCP client first)".into();
    }
    if lower.contains("enotempty")
        || lower.contains("npm error")
        || lower.contains("enoent")
        || lower.contains("package was not found")
    {
        return "package install failed (npx/uvx); try reinstalling the MCP package".into();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "startup timed out".into();
    }
    if lower.contains("connection refused") || lower.contains("connect error") {
        return "connection refused".into();
    }
    // Collapse multi-line noise.
    let one_line: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(" · ");
    if one_line.chars().count() > 160 {
        one_line.chars().take(157).collect::<String>() + "…"
    } else if one_line.is_empty() {
        "failed".into()
    } else {
        one_line
    }
}

async fn connect_server(
    name: &str,
    cfg: &McpServerConfig,
    max_output_bytes: usize,
) -> Result<LiveServer> {
    cfg.validate(name)?;
    let startup = cfg.startup_timeout();
    let tool_timeout = cfg.tool_timeout();

    if cfg.is_stdio() {
        connect_stdio(name, cfg, startup, tool_timeout, max_output_bytes).await
    } else {
        connect_http(name, cfg, startup, tool_timeout, max_output_bytes).await
    }
}

async fn connect_stdio(
    name: &str,
    cfg: &McpServerConfig,
    startup: Duration,
    tool_timeout: Duration,
    max_output_bytes: usize,
) -> Result<LiveServer> {
    let command = cfg.command.as_ref().expect("validated stdio");
    let args = &cfg.args;

    let cmd = tokio::process::Command::new(command).configure(|c| {
        for a in args {
            c.arg(a);
        }
        for (k, v) in &cfg.env {
            c.env(k, v);
        }
        if let Some(cwd) = &cfg.cwd {
            c.current_dir(cwd);
        }
        // Quiet package managers so npx/uvx don't spam the TUI over alt-screen.
        c.env("NPM_CONFIG_LOGLEVEL", "silent");
        c.env("npm_config_update_notifier", "false");
        c.env("NPM_CONFIG_UPDATE_NOTIFIER", "false");
        c.env("NO_UPDATE_NOTIFIER", "1");
    });

    // Default builder uses stderr=inherit which **corrupts the TUI**. Force null.
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| McpError::server(name, format!("spawn `{command}`: {e}")))?;

    let service = tokio::time::timeout(startup, ().serve(transport))
        .await
        .map_err(|_| {
            McpError::server(
                name,
                format!("startup timed out after {}s", startup.as_secs()),
            )
        })?
        .map_err(|e| McpError::server(name, format!("handshake failed: {e}")))?;

    finish_connect(name, "stdio", service, cfg, tool_timeout, max_output_bytes).await
}

async fn connect_http(
    name: &str,
    cfg: &McpServerConfig,
    startup: Duration,
    tool_timeout: Duration,
    max_output_bytes: usize,
) -> Result<LiveServer> {
    let url = cfg.url.as_ref().expect("validated http").clone();

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(token) = &cfg.auth_token {
        config = config.auth_header(token.clone());
    }
    if !cfg.headers.is_empty() {
        use http::{HeaderName, HeaderValue};
        use std::collections::HashMap;
        let mut headers = HashMap::new();
        for (k, v) in &cfg.headers {
            let key = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::server(name, format!("invalid header name `{k}`: {e}")))?;
            let val = HeaderValue::from_str(v).map_err(|e| {
                McpError::server(name, format!("invalid header value for `{k}`: {e}"))
            })?;
            headers.insert(key, val);
        }
        config = config.custom_headers(headers);
    }

    let transport = StreamableHttpClientTransport::from_config(config);
    let service = tokio::time::timeout(startup, ().serve(transport))
        .await
        .map_err(|_| {
            McpError::server(
                name,
                format!("startup timed out after {}s", startup.as_secs()),
            )
        })?
        .map_err(|e| McpError::server(name, format!("handshake failed: {e}")))?;

    finish_connect(name, "http", service, cfg, tool_timeout, max_output_bytes).await
}

async fn finish_connect(
    name: &str,
    transport: &str,
    service: RunningService<RoleClient, ()>,
    cfg: &McpServerConfig,
    tool_timeout: Duration,
    max_output_bytes: usize,
) -> Result<LiveServer> {
    let peer = service.peer().clone();
    // Optional human-readable server blurb for deferred-mode announcements.
    let description = peer.peer_info().and_then(|info| {
        let from_instructions = info
            .instructions
            .as_ref()
            .map(|s| s.as_str())
            .map(sanitize_description)
            .map(|s| truncate_description(&s))
            .filter(|s| !s.is_empty());
        from_instructions.or_else(|| {
            info.server_info
                .title
                .clone()
                .map(|s| truncate_description(&sanitize_description(&s)))
                .filter(|s| !s.is_empty())
        })
    });

    let listed = peer
        .list_all_tools()
        .await
        .map_err(|e| McpError::server(name, format!("tools/list failed: {e}")))?;

    let allow = cfg.tools.as_deref();
    let tools = tools_from_list(name, listed, allow, peer, tool_timeout, max_output_bytes);

    Ok(LiveServer {
        name: name.to_string(),
        description,
        _service: service,
        tools,
        transport: transport.to_string(),
    })
}

/// Probe a single server without retaining the connection (for doctor).
pub async fn probe_server(name: &str, cfg: &McpServerConfig) -> ServerHealth {
    let mut cfg = cfg.clone();
    cfg.expand_strings();
    match connect_server(name, &cfg, DEFAULT_MAX_OUTPUT_BYTES).await {
        Ok(live) => ServerHealth {
            name: name.to_string(),
            transport: live.transport,
            ok: true,
            message: "ok".into(),
            tool_count: live.tools.len(),
            tools: live.tools.iter().map(|t| t.definition().name).collect(),
            source: None,
        },
        Err(e) => ServerHealth {
            name: name.to_string(),
            transport: if cfg.is_http() {
                "http".into()
            } else {
                "stdio".into()
            },
            ok: false,
            message: e.to_string(),
            tool_count: 0,
            tools: vec![],
            source: None,
        },
    }
}
