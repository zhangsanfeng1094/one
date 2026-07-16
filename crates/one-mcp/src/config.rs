//! MCP configuration loading (multi-source merge).
//!
//! **One native format is JSON only** (`mcp.json`).
//!
//! Priority (high → low; same name = full replacement, not field merge):
//! 1. One project: `.one/mcp.json` (cwd → git root, cwd wins)
//! 2. One user: `~/.one/agent/mcp.json`
//! 3. Codex: `~/.codex/config.toml` `[mcp_servers]` (read-only TOML compat)
//! 4. Claude: `~/.claude.json`
//! 5. Cursor: `.cursor/mcp.json` / `~/.cursor/mcp.json`
//! 6. Standard: project `.mcp.json`
//!
//! Implemented by merging **low → high** so later layers win.
//! Compat sources (Claude / Cursor / Codex / `.mcp.json`) are read-only —
//! not a second One native format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{McpError, Result};

/// Default max tool-result size returned to the model (bytes).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 20_000;

/// Default per-tool call timeout (seconds).
pub const DEFAULT_TOOL_TIMEOUT_SEC: u64 = 120;

/// Default server startup timeout (seconds).
pub const DEFAULT_STARTUP_TIMEOUT_SEC: u64 = 30;

/// Where a server entry came from (for `one mcp list` / doctor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSourceKind {
    OneUser,
    OneProject,
    Codex,
    Claude,
    Cursor,
    StandardMcpJson,
}

impl ConfigSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneUser => "one-user",
            Self::OneProject => "one-project",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::StandardMcpJson => "mcp.json",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigSourceReport {
    pub kind: ConfigSourceKind,
    pub path: PathBuf,
    pub server_names: Vec<String>,
}

/// Effective load result.
#[derive(Debug, Clone)]
pub struct LoadedMcpConfig {
    pub config: McpConfig,
    /// Provenance of the winning entry for each server name.
    pub server_sources: BTreeMap<String, ConfigSourceKind>,
    pub sources: Vec<ConfigSourceReport>,
}

/// Root MCP config document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfig {
    #[serde(default, alias = "mcp_servers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,

    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,

    /// Server names the user turned off (UI / `one mcp`).
    /// Owned by user `mcp.json` only; applied after multi-source merge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_servers: Vec<String>,
}

fn default_max_output() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}

impl McpConfig {
    pub fn empty() -> Self {
        Self {
            mcp_servers: BTreeMap::new(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            disabled_servers: Vec::new(),
        }
    }

    /// Merge `other` on top (other wins on same server name — full replace).
    /// `disabled_servers` is **not** merged here — user list is applied separately.
    pub fn merge(mut self, other: McpConfig) -> Self {
        for (k, v) in other.mcp_servers {
            self.mcp_servers.insert(k, v);
        }
        if other.max_output_bytes != DEFAULT_MAX_OUTPUT_BYTES
            || self.max_output_bytes == DEFAULT_MAX_OUTPUT_BYTES
        {
            // Prefer explicit non-default from higher layer when present.
            // Always take other's max if it was set via a file that parsed it.
            self.max_output_bytes = other.max_output_bytes;
        }
        self
    }

    pub fn enabled_servers(&self) -> impl Iterator<Item = (&String, &McpServerConfig)> {
        self.mcp_servers
            .iter()
            .filter(|(_, s)| s.enabled.unwrap_or(true))
    }
}

/// One MCP server entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Optional transport type hint: `http` | `sse` (accepted for Grok/Cursor compat).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub transport_type: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    /// Name of env var holding a bearer token (Cursor/Grok style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeouts: Option<BTreeMap<String, u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl McpServerConfig {
    pub fn is_stdio(&self) -> bool {
        self.command.is_some()
    }

    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }

    pub fn validate(&self, name: &str) -> Result<()> {
        match (self.command.is_some(), self.url.is_some()) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(McpError::config(format!(
                "server `{name}`: set either `command` or `url`, not both"
            ))),
            (false, false) => Err(McpError::config(format!(
                "server `{name}`: need `command` (stdio) or `url` (http)"
            ))),
        }
    }

    pub fn startup_timeout(&self) -> std::time::Duration {
        // Env overrides (Grok / Claude Code compat), then per-server, then default.
        if let Some(secs) = env_startup_timeout_secs() {
            return std::time::Duration::from_secs(
                self.startup_timeout_sec.unwrap_or(secs),
            );
        }
        std::time::Duration::from_secs(
            self.startup_timeout_sec
                .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SEC),
        )
    }

    pub fn tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.tool_timeout_sec.unwrap_or(DEFAULT_TOOL_TIMEOUT_SEC))
    }

    /// Expand env placeholders in string fields (Grok `expand_strings`).
    pub fn expand_strings(&mut self) {
        if let Some(c) = &mut self.command {
            *c = expand_env(c);
        }
        for a in &mut self.args {
            *a = expand_env(a);
        }
        for v in self.env.values_mut() {
            *v = expand_env(v);
        }
        if let Some(u) = &mut self.url {
            *u = expand_env(u);
        }
        for v in self.headers.values_mut() {
            *v = expand_env(v);
        }
        if let Some(t) = &mut self.auth_token {
            *t = expand_env(t);
        }
        if let Some(c) = &mut self.cwd {
            *c = expand_env(c);
        }
        // Resolve bearer_token_env_var into headers if set.
        if let Some(var) = &self.bearer_token_env_var {
            if let Ok(token) = std::env::var(var) {
                self.headers
                    .entry("Authorization".into())
                    .or_insert_with(|| format!("Bearer {token}"));
            }
        }
    }
}

fn env_startup_timeout_secs() -> Option<u64> {
    // Per-server still wins when set; this is the global default base.
    if let Ok(s) = std::env::var("ONE_MCP_STARTUP_TIMEOUT_SECS")
        .or_else(|_| std::env::var("GROK_MCP_STARTUP_TIMEOUT_SECS"))
    {
        return s.parse().ok();
    }
    // Claude Code: MCP_TIMEOUT in milliseconds
    if let Ok(ms) = std::env::var("MCP_TIMEOUT") {
        if let Ok(n) = ms.parse::<u64>() {
            return Some(n.div_ceil(1000).max(1));
        }
    }
    None
}

/// Expand `${VAR}` / `$VAR` in a string from process environment.
pub fn expand_env(input: &str) -> String {
    let re_braced = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("regex");
    let s = re_braced.replace_all(input, |caps: &regex::Captures| {
        std::env::var(caps.get(1).unwrap().as_str()).unwrap_or_default()
    });
    let re_plain = regex::Regex::new(r"\$([A-Za-z_][A-Za-z0-9_]*)").expect("regex");
    re_plain
        .replace_all(&s, |caps: &regex::Captures| {
            std::env::var(caps.get(1).unwrap().as_str()).unwrap_or_default()
        })
        .into_owned()
}

pub fn user_mcp_path() -> PathBuf {
    crate::paths::agent_dir().join("mcp.json")
}

pub fn project_mcp_path(cwd: &Path) -> PathBuf {
    cwd.join(".one").join("mcp.json")
}

fn compat_enabled(env_name: &str, default: bool) -> bool {
    match std::env::var(env_name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "enabled"
        ),
        Err(_) => default,
    }
}

/// Grok-style effective config for `cwd`.
pub fn load_effective(cwd: &Path) -> Result<LoadedMcpConfig> {
    let mut cfg = McpConfig::empty();
    let mut server_sources: BTreeMap<String, ConfigSourceKind> = BTreeMap::new();
    let mut sources = Vec::new();

    // Layer 1 (lowest): standard `.mcp.json` along cwd → git root (parent first).
    for path in walk_chain_files(cwd, |dir| dir.join(".mcp.json")) {
        if let Ok(layer) = load_file(&path) {
            record_layer(
                &mut cfg,
                &mut server_sources,
                &mut sources,
                ConfigSourceKind::StandardMcpJson,
                &path,
                layer,
            );
        }
    }

    // Layer 2: Cursor
    if compat_enabled("ONE_CURSOR_MCPS_ENABLED", true)
        && compat_enabled("GROK_CURSOR_MCPS_ENABLED", true)
    {
        let user_cursor = dirs_home().join(".cursor").join("mcp.json");
        if user_cursor.is_file() {
            if let Ok(layer) = load_file(&user_cursor) {
                record_layer(
                    &mut cfg,
                    &mut server_sources,
                    &mut sources,
                    ConfigSourceKind::Cursor,
                    &user_cursor,
                    layer,
                );
            }
        }
        for path in walk_chain_files(cwd, |dir| dir.join(".cursor").join("mcp.json")) {
            if let Ok(layer) = load_file(&path) {
                record_layer(
                    &mut cfg,
                    &mut server_sources,
                    &mut sources,
                    ConfigSourceKind::Cursor,
                    &path,
                    layer,
                );
            }
        }
    }

    // Layer 3: Claude
    if compat_enabled("ONE_CLAUDE_MCPS_ENABLED", true)
        && compat_enabled("GROK_CLAUDE_MCPS_ENABLED", true)
    {
        let claude_path = dirs_home().join(".claude.json");
        if claude_path.is_file() {
            if let Ok(layer) = load_claude_json(&claude_path, cwd) {
                record_layer(
                    &mut cfg,
                    &mut server_sources,
                    &mut sources,
                    ConfigSourceKind::Claude,
                    &claude_path,
                    layer,
                );
            }
        }
    }

    // Layer 4: Codex (read-only TOML — not One native)
    if compat_enabled("ONE_CODEX_MCPS_ENABLED", true) {
        let codex_path = dirs_home().join(".codex").join("config.toml");
        if codex_path.is_file() {
            match load_codex_toml(&codex_path) {
                Ok(layer) if !layer.mcp_servers.is_empty() => {
                    record_layer(
                        &mut cfg,
                        &mut server_sources,
                        &mut sources,
                        ConfigSourceKind::Codex,
                        &codex_path,
                        layer,
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        path = %codex_path.display(),
                        error = %e,
                        "failed to read Codex MCP config"
                    );
                }
            }
        }
    }

    // Layer 5: One user native (JSON only)
    let user_json = user_mcp_path();
    if user_json.is_file() {
        if let Ok(layer) = load_file(&user_json) {
            record_layer(
                &mut cfg,
                &mut server_sources,
                &mut sources,
                ConfigSourceKind::OneUser,
                &user_json,
                layer,
            );
        }
    }

    // Layer 6 (highest): One project `.one/mcp.json` chain parent→cwd
    for dir in walk_dirs_to_git_root(cwd) {
        let json = dir.join(".one").join("mcp.json");
        if json.is_file() {
            if let Ok(layer) = load_file(&json) {
                record_layer(
                    &mut cfg,
                    &mut server_sources,
                    &mut sources,
                    ConfigSourceKind::OneProject,
                    &json,
                    layer,
                );
            }
        }
    }

    // Env max output override (highest)
    if let Ok(v) = std::env::var("ONE_MAX_MCP_OUTPUT_BYTES")
        .or_else(|_| std::env::var("MAX_MCP_OUTPUT_BYTES"))
        .or_else(|_| std::env::var("GROK_MAX_MCP_OUTPUT_BYTES"))
    {
        if let Ok(n) = v.parse::<usize>() {
            cfg.max_output_bytes = n;
        }
    }

    // User-owned disable list (One UI / mcp.json) — not from Claude/Codex.
    let user_disabled = load_user_or_empty()
        .map(|u| u.disabled_servers)
        .unwrap_or_default();
    cfg.disabled_servers = user_disabled;
    for name in &cfg.disabled_servers {
        if let Some(s) = cfg.mcp_servers.get_mut(name) {
            s.enabled = Some(false);
        }
    }

    // Expand env + validate
    for (name, server) in cfg.mcp_servers.iter_mut() {
        server.expand_strings();
        server.validate(name)?;
    }

    Ok(LoadedMcpConfig {
        config: cfg,
        server_sources,
        sources,
    })
}

/// Persist enable/disable for a server name (user `mcp.json`).
///
/// Foreign (Claude/Codex) servers are toggled via `disabledServers` list.
/// One-owned entries also get `enabled` flipped when present.
pub fn set_server_disabled_persistent(name: &str, disabled: bool) -> Result<()> {
    let mut cfg = load_user_or_empty()?;
    if disabled {
        if !cfg.disabled_servers.iter().any(|n| n == name) {
            cfg.disabled_servers.push(name.to_string());
        }
        if let Some(s) = cfg.mcp_servers.get_mut(name) {
            s.enabled = Some(false);
        }
    } else {
        cfg.disabled_servers.retain(|n| n != name);
        if let Some(s) = cfg.mcp_servers.get_mut(name) {
            s.enabled = Some(true);
        }
    }
    save_user_config(&cfg)
}

/// Back-compat alias.
pub fn load_merged(cwd: &Path) -> Result<McpConfig> {
    Ok(load_effective(cwd)?.config)
}

fn record_layer(
    cfg: &mut McpConfig,
    server_sources: &mut BTreeMap<String, ConfigSourceKind>,
    sources: &mut Vec<ConfigSourceReport>,
    kind: ConfigSourceKind,
    path: &Path,
    layer: McpConfig,
) {
    let names: Vec<String> = layer.mcp_servers.keys().cloned().collect();
    if names.is_empty() && layer.max_output_bytes == DEFAULT_MAX_OUTPUT_BYTES {
        return;
    }
    for n in &names {
        server_sources.insert(n.clone(), kind);
    }
    sources.push(ConfigSourceReport {
        kind,
        path: path.to_path_buf(),
        server_names: names,
    });
    // max_output: take layer value if it differs from default OR always merge servers
    let max = layer.max_output_bytes;
    *cfg = std::mem::take(cfg).merge(layer);
    if max != DEFAULT_MAX_OUTPUT_BYTES {
        cfg.max_output_bytes = max;
    }
}

/// Dirs from git-root/parent chain up to cwd (inclusive), root first.
fn walk_dirs_to_git_root(cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut cur = cwd.to_path_buf();
    loop {
        chain.push(cur.clone());
        if cur.join(".git").exists() {
            break;
        }
        if !cur.pop() {
            break;
        }
    }
    chain.reverse(); // root … cwd
    chain
}

/// Files along the chain (root → cwd) that exist.
fn walk_chain_files(cwd: &Path, make: impl Fn(&Path) -> PathBuf) -> Vec<PathBuf> {
    walk_dirs_to_git_root(cwd)
        .into_iter()
        .map(|d| make(&d))
        .filter(|p| p.is_file())
        .collect()
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn load_file(path: &Path) -> Result<McpConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| McpError::config(format!("read {}: {e}", path.display())))?;
    parse_config_json(&text)
}

pub fn parse_config_json(text: &str) -> Result<McpConfig> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| McpError::config(format!("invalid JSON: {e}")))?;

    if value.get("mcpServers").is_some()
        || value.get("mcp_servers").is_some()
        || value.get("maxOutputBytes").is_some()
        || value.get("max_output_bytes").is_some()
    {
        return serde_json::from_value(value)
            .map_err(|e| McpError::config(format!("invalid mcp config: {e}")));
    }

    if value.is_object() {
        let servers: BTreeMap<String, McpServerConfig> = serde_json::from_value(value)
            .map_err(|e| McpError::config(format!("invalid mcp servers map: {e}")))?;
        return Ok(McpConfig {
            mcp_servers: servers,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            disabled_servers: Vec::new(),
        });
    }

    Err(McpError::config("mcp config must be a JSON object"))
}

fn load_claude_json(path: &Path, cwd: &Path) -> Result<McpConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| McpError::config(format!("read {}: {e}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| McpError::config(format!("claude.json: {e}")))?;

    let mut cfg = McpConfig::empty();

    // Global mcpServers
    if let Some(servers) = value.get("mcpServers") {
        let map: BTreeMap<String, McpServerConfig> = serde_json::from_value(servers.clone())
            .map_err(|e| McpError::config(format!("claude mcpServers: {e}")))?;
        cfg.mcp_servers = map;
    }

    // Project-specific: longest matching projects.<path>
    if let Some(projects) = value.get("projects").and_then(|p| p.as_object()) {
        let cwd_s = cwd
            .canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .to_string();
        let mut best: Option<(usize, BTreeMap<String, McpServerConfig>)> = None;
        for (proj_path, proj_val) in projects {
            let canon = PathBuf::from(proj_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(proj_path));
            let p = canon.to_string_lossy();
            if cwd_s == p || cwd_s.starts_with(&(p.to_string() + "/")) {
                if let Some(ms) = proj_val.get("mcpServers") {
                    if let Ok(map) =
                        serde_json::from_value::<BTreeMap<String, McpServerConfig>>(ms.clone())
                    {
                        let len = p.len();
                        if best.as_ref().map(|(l, _)| len > *l).unwrap_or(true) {
                            best = Some((len, map));
                        }
                    }
                }
            }
        }
        if let Some((_, map)) = best {
            // Project servers replace global same names
            for (k, v) in map {
                cfg.mcp_servers.insert(k, v);
            }
        }
    }

    Ok(cfg)
}

/// Read-only Codex compat: `~/.codex/config.toml` `[mcp_servers.<name>]`.
///
/// Codex nests per-tool approval under `tools.*` which is **not** our allowlist
/// `tools: Vec<String>`, so we parse via `toml::Value` and only map known fields.
pub fn load_codex_toml(path: &Path) -> Result<McpConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| McpError::config(format!("read {}: {e}", path.display())))?;
    parse_codex_toml(&text)
}

pub fn parse_codex_toml(text: &str) -> Result<McpConfig> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|e| McpError::config(format!("codex config.toml: {e}")))?;

    let Some(servers) = value.get("mcp_servers").and_then(|v| v.as_table()) else {
        return Ok(McpConfig::empty());
    };

    let mut cfg = McpConfig::empty();
    for (name, entry) in servers {
        let Some(table) = entry.as_table() else {
            tracing::warn!(server = %name, "codex mcp_servers entry is not a table; skip");
            continue;
        };
        match codex_table_to_server(table) {
            Ok(server) => {
                if let Err(e) = server.validate(name) {
                    tracing::warn!(server = %name, error = %e, "codex MCP entry invalid; skip");
                    continue;
                }
                cfg.mcp_servers.insert(name.clone(), server);
            }
            Err(e) => {
                tracing::warn!(server = %name, error = %e, "codex MCP entry skip");
            }
        }
    }
    Ok(cfg)
}

fn codex_table_to_server(table: &toml::map::Map<String, toml::Value>) -> Result<McpServerConfig> {
    let command = table
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let url = table
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let args = table
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut env = BTreeMap::new();
    if let Some(env_tbl) = table.get("env").and_then(|v| v.as_table()) {
        for (k, v) in env_tbl {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            } else if let Some(n) = v.as_integer() {
                env.insert(k.clone(), n.to_string());
            } else if let Some(b) = v.as_bool() {
                env.insert(k.clone(), b.to_string());
            }
        }
    }

    let mut headers = BTreeMap::new();
    if let Some(h_tbl) = table.get("headers").and_then(|v| v.as_table()) {
        for (k, v) in h_tbl {
            if let Some(s) = v.as_str() {
                headers.insert(k.clone(), s.to_string());
            }
        }
    }

    let enabled = table.get("enabled").and_then(|v| v.as_bool());
    let startup_timeout_sec = table
        .get("startup_timeout_sec")
        .and_then(|v| v.as_integer())
        .and_then(|n| u64::try_from(n).ok());
    let tool_timeout_sec = table
        .get("tool_timeout_sec")
        .and_then(|v| v.as_integer())
        .and_then(|n| u64::try_from(n).ok());
    let cwd = table
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let bearer_token_env_var = table
        .get("bearer_token_env_var")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let auth_token = table
        .get("auth_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let transport_type = table
        .get("type")
        .or_else(|| table.get("transport"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Codex `tools` is per-tool approval map — intentionally ignored.
    // Our allowlist is only taken if tools is a string array.
    let tools = table.get("tools").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
    });
    let tools = tools.filter(|t| !t.is_empty());

    Ok(McpServerConfig {
        command,
        args,
        env,
        url,
        transport_type,
        headers,
        auth_token,
        bearer_token_env_var,
        enabled,
        startup_timeout_sec,
        tool_timeout_sec,
        tool_timeouts: None,
        tools,
        cwd,
    })
}

pub fn save_user_config(cfg: &McpConfig) -> Result<()> {
    let path = user_mcp_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| McpError::config(format!("serialize: {e}")))?;
    std::fs::write(&path, text + "\n")?;
    Ok(())
}

pub fn load_user_or_empty() -> Result<McpConfig> {
    let path = user_mcp_path();
    if path.is_file() {
        load_file(&path)
    } else {
        Ok(McpConfig::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_style() {
        let json = r#"{
            "mcpServers": {
                "fs": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
                }
            }
        }"#;
        let cfg = parse_config_json(json).unwrap();
        assert!(cfg.mcp_servers.contains_key("fs"));
    }

    #[test]
    fn expand_env_var() {
        std::env::set_var("ONE_TEST_MCP_TOKEN", "secret123");
        assert_eq!(
            expand_env("Bearer ${ONE_TEST_MCP_TOKEN}"),
            "Bearer secret123"
        );
        std::env::remove_var("ONE_TEST_MCP_TOKEN");
    }

    #[test]
    fn parse_codex_toml_ignores_tool_approvals() {
        let toml = r#"
model = "gpt-5.5"

[mcp_servers.deepwiki]
url = "https://mcp.deepwiki.com/mcp"

[mcp_servers.n8n]
command = "npx"
args = ["n8n-mcp"]
[mcp_servers.n8n.env]
N8N_API_KEY = "secret"
MCP_MODE = "stdio"

[mcp_servers.context-mode]
command = "context-mode"
[mcp_servers.context-mode.tools.ctx_execute]
approval_mode = "approve"
"#;
        let cfg = parse_codex_toml(toml).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 3);
        assert!(cfg.mcp_servers["deepwiki"].is_http());
        assert_eq!(cfg.mcp_servers["n8n"].command.as_deref(), Some("npx"));
        assert_eq!(cfg.mcp_servers["n8n"].args, vec!["n8n-mcp"]);
        assert_eq!(
            cfg.mcp_servers["n8n"].env.get("N8N_API_KEY").map(String::as_str),
            Some("secret")
        );
        // tools approval map must not become allowlist / must not break parse
        assert!(cfg.mcp_servers["context-mode"].tools.is_none());
        assert_eq!(
            cfg.mcp_servers["context-mode"].command.as_deref(),
            Some("context-mode")
        );
    }

    #[test]
    fn merge_full_replace() {
        let mut a = McpConfig::empty();
        a.mcp_servers.insert(
            "x".into(),
            McpServerConfig {
                command: Some("a".into()),
                args: vec!["1".into()],
                env: BTreeMap::new(),
                url: None,
                transport_type: None,
                headers: BTreeMap::new(),
                auth_token: None,
                bearer_token_env_var: None,
                enabled: Some(true),
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                tool_timeouts: None,
                tools: None,
                cwd: None,
            },
        );
        let mut b = McpConfig::empty();
        b.mcp_servers.insert(
            "x".into(),
            McpServerConfig {
                command: Some("b".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                transport_type: None,
                headers: BTreeMap::new(),
                auth_token: None,
                bearer_token_env_var: None,
                enabled: Some(false),
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                tool_timeouts: None,
                tools: None,
                cwd: None,
            },
        );
        let m = a.merge(b);
        assert_eq!(m.mcp_servers["x"].command.as_deref(), Some("b"));
        assert!(m.mcp_servers["x"].args.is_empty());
    }

    #[test]
    fn walk_chain_includes_cwd() {
        let dirs = walk_dirs_to_git_root(Path::new("/tmp"));
        assert!(!dirs.is_empty());
        assert_eq!(dirs.last().unwrap(), Path::new("/tmp"));
    }
}
