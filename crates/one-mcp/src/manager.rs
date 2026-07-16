//! Connect MCP servers and expose their tools.
//!
//! **Async load (Grok-style):**
//! - Config is read synchronously (disk only).
//! - Connections run in a background task (`buffer_unordered(8)`).
//! - Session / TUI start is not blocked on cold `npx` downloads.
//! - Each finished server bumps a generation counter; the host re-syncs
//!   tools onto the Agent before the next prompt.
//! - `/new` keeps the live connection pool (shared across conversations).

use std::collections::HashSet;
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

use crate::config::{
    load_effective, set_server_disabled_persistent, ConfigSourceKind, LoadedMcpConfig, McpConfig,
    McpServerConfig, DEFAULT_MAX_OUTPUT_BYTES,
};
use crate::error::{McpError, Result};
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
        let total = self.shared.config.mcp_servers.len();
        if total == 0 {
            return None;
        }
        let disabled = self.shared.disabled_names.read();
        let live = self.shared.live.read();
        let failures = self.shared.failures.read();
        let loading = self.shared.loading.load(Ordering::SeqCst)
            || self.shared.pending.load(Ordering::SeqCst) > 0;

        let ready = live
            .iter()
            .filter(|s| !disabled.contains(&s.name))
            .count();
        let failed = failures
            .iter()
            .filter(|(n, _)| !disabled.contains(n))
            .count();
        let enabled_total = self
            .shared
            .config
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
    _service: RunningService<RoleClient, ()>,
    tools: Vec<Arc<dyn Tool>>,
    transport: String,
}

struct SharedState {
    config: McpConfig,
    server_sources: std::collections::BTreeMap<String, ConfigSourceKind>,
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
                config: McpConfig::empty(),
                server_sources: Default::default(),
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

    pub fn config(&self) -> &McpConfig {
        &self.shared.config
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
        if self.shared.config.mcp_servers.is_empty() {
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
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.shared.tools.read().clone()
    }

    pub fn tool_count(&self) -> usize {
        self.shared.tools.read().len()
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
        self.shared.server_sources.get(name).copied()
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
        let disabled_set: HashSet<String> = loaded.config.disabled_servers.iter().cloned().collect();
        let n_enabled = loaded.config.enabled_servers().count();
        let shared = Arc::new(SharedState {
            config: loaded.config.clone(),
            server_sources: loaded.server_sources.clone(),
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

        let mut rows = Vec::new();
        for (name, cfg) in &self.shared.config.mcp_servers {
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
        let ok = rows.iter().filter(|r| r.enabled && r.status == "ok").count();
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

    /// Toggle one server on/off (persists + reconnects or drops tools).
    pub async fn set_server_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        if self.disabled {
            return Err(McpError::other("MCP is disabled for this process (--no-mcp)"));
        }
        if !self.shared.config.mcp_servers.contains_key(name) {
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

        // Update in-memory config enabled flag
        // (config is behind Arc without mut — store only in disabled_names for runtime)

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
                    .mcp_servers
                    .get(name)
                    .cloned()
                    .ok_or_else(|| McpError::other("server vanished"))?;
                let shared = Arc::clone(&self.shared);
                let name = name.to_string();
                self.shared.pending.fetch_add(1, Ordering::SeqCst);
                self.shared.loading.store(true, Ordering::SeqCst);
                tokio::spawn(async move {
                    connect_one_into(shared, name, cfg).await;
                });
            }
        } else {
            // Keep connection for fast re-enable, but drop tools from the agent set.
            rebuild_tools_from_live(&self.shared);
        }

        info!(server = %name, enabled, "MCP server toggle");
        Ok(())
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
                source: self
                    .server_source(&s.name)
                    .map(|k| k.as_str().to_string()),
            });
        }
        let live_names: std::collections::HashSet<String> =
            live.iter().map(|s| s.name.clone()).collect();
        drop(live);

        for (name, msg) in self.shared.failures.read().iter() {
            out.push(ServerHealth {
                name: name.clone(),
                transport: self
                    .shared
                    .config
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
            for (name, cfg) in self.shared.config.enabled_servers() {
                if live_names.contains(name) {
                    continue;
                }
                if self
                    .shared
                    .failures
                    .read()
                    .iter()
                    .any(|(n, _)| n == name)
                {
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

    let max_out = if shared.config.max_output_bytes == 0 {
        DEFAULT_MAX_OUTPUT_BYTES
    } else {
        shared.config.max_output_bytes
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
        return "needs OAuth / login (set token or authenticate the host MCP client first)"
            .into();
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
            McpError::server(name, format!("startup timed out after {}s", startup.as_secs()))
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
            let key = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                McpError::server(name, format!("invalid header name `{k}`: {e}"))
            })?;
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
            McpError::server(name, format!("startup timed out after {}s", startup.as_secs()))
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
    let listed = peer
        .list_all_tools()
        .await
        .map_err(|e| McpError::server(name, format!("tools/list failed: {e}")))?;

    let allow = cfg.tools.as_deref();
    let tools = tools_from_list(
        name,
        listed,
        allow,
        peer,
        tool_timeout,
        max_output_bytes,
    );

    Ok(LiveServer {
        name: name.to_string(),
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

