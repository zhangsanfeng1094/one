//! `one mcp …` management commands.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use one_mcp::{
    load_user_or_empty, probe_server, save_user_config, user_mcp_path, McpServerConfig,
};

#[derive(Debug, Clone, Parser)]
#[command(about = "Manage MCP servers (platform foundation)")]
pub struct McpCli {
    #[command(subcommand)]
    pub action: McpAction,
}

#[derive(Debug, Clone, Subcommand)]
pub enum McpAction {
    /// List configured servers (user + project merge view when --cwd set)
    List {
        /// Also show project `.one/mcp.json` merged result
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Add a server to ~/.one/agent/mcp.json
    Add {
        /// Server name (letters, numbers, `_`, `-`)
        name: String,
        /// Transport kind
        #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
        transport: McpTransport,
        /// HTTP URL (when --transport http)
        #[arg(long)]
        url: Option<String>,
        /// Env KEY=VALUE (repeatable, stdio)
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Header KEY=VALUE (repeatable, http)
        #[arg(long = "header", value_name = "KEY=VALUE")]
        headers: Vec<String>,
        /// Command and args after `--` for stdio
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Remove a server from user config
    Remove {
        name: String,
    },
    /// Diagnose connectivity for configured servers
    Doctor {
        /// Optional single server name
        name: Option<String>,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTransport {
    Stdio,
    Http,
}

pub async fn run_mcp(cli: McpCli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.action {
        McpAction::List { cwd, json } => cmd_list(&cwd, json).await,
        McpAction::Add {
            name,
            transport,
            url,
            env,
            headers,
            command,
        } => cmd_add(name, transport, url, env, headers, command).await,
        McpAction::Remove { name } => cmd_remove(name).await,
        McpAction::Doctor { name, cwd, json } => cmd_doctor(name, &cwd, json).await,
    }
}

async fn cmd_list(cwd: &std::path::Path, as_json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = one_mcp::load_effective(cwd)?;
    let cfg = &loaded.config;
    if as_json {
        let sources: Vec<_> = loaded
            .sources
            .iter()
            .map(|s| {
                serde_json::json!({
                    "kind": s.kind.as_str(),
                    "path": s.path.display().to_string(),
                    "servers": s.server_names,
                })
            })
            .collect();
        let provenance: serde_json::Map<String, serde_json::Value> = loaded
            .server_sources
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v.as_str())))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "config": cfg,
                "sources": sources,
                "serverSources": provenance,
            }))?
        );
        return Ok(());
    }
    if cfg.mcp_servers.is_empty() {
        println!("No MCP servers configured (after multi-source merge).");
        println!("  one user:     {}", user_mcp_path().display());
        println!(
            "  one project:  {}",
            one_mcp::project_mcp_path(cwd).display()
        );
        println!(
            "  also scans: Codex ~/.codex/config.toml, Claude, Cursor, project .mcp.json"
        );
        println!(
            "Add one: one mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path"
        );
        return Ok(());
    }
    if !loaded.sources.is_empty() {
        println!("Sources (low → high priority):");
        for s in &loaded.sources {
            println!(
                "  [{:<12}] {}  ({})",
                s.kind.as_str(),
                s.path.display(),
                s.server_names.join(", ")
            );
        }
        println!();
    }
    println!(
        "{:<16} {:<8} {:<10} {:<8} {}",
        "NAME", "ENABLED", "SOURCE", "TYPE", "TARGET"
    );
    println!("{}", "-".repeat(88));
    for (name, s) in &cfg.mcp_servers {
        let enabled = s.enabled.unwrap_or(true);
        let src = loaded
            .server_sources
            .get(name)
            .map(|k| k.as_str())
            .unwrap_or("?");
        let (kind, target) = if let Some(url) = &s.url {
            ("http", url.clone())
        } else {
            let cmd = s.command.clone().unwrap_or_default();
            let mut t = cmd;
            if !s.args.is_empty() {
                t.push(' ');
                t.push_str(&s.args.join(" "));
            }
            ("stdio", t)
        };
        println!(
            "{:<16} {:<8} {:<10} {:<8} {}",
            name,
            if enabled { "yes" } else { "no" },
            src,
            kind,
            target
        );
    }
    Ok(())
}

async fn cmd_add(
    name: String,
    transport: McpTransport,
    url: Option<String>,
    env: Vec<String>,
    headers: Vec<String>,
    command: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_name(&name)?;
    let mut cfg = load_user_or_empty()?;

    let server = match transport {
        McpTransport::Stdio => {
            if command.is_empty() {
                return Err("stdio transport needs a command after `--`\n  example: one mcp add fs -- npx -y @modelcontextprotocol/server-filesystem /tmp".into());
            }
            let mut env_map = BTreeMap::new();
            for pair in env {
                let (k, v) = split_kv(&pair)?;
                env_map.insert(k, v);
            }
            McpServerConfig {
                command: Some(command[0].clone()),
                args: command[1..].to_vec(),
                env: env_map,
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
            }
        }
        McpTransport::Http => {
            let url = url.ok_or("--url is required for --transport http")?;
            let mut header_map = BTreeMap::new();
            for pair in headers {
                let (k, v) = split_kv(&pair)?;
                header_map.insert(k, v);
            }
            McpServerConfig {
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                url: Some(url),
                transport_type: None,
                headers: header_map,
                auth_token: None,
                bearer_token_env_var: None,
                enabled: Some(true),
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                tool_timeouts: None,
                tools: None,
                cwd: None,
            }
        }
    };
    server.validate(&name)?;
    cfg.mcp_servers.insert(name.clone(), server);
    save_user_config(&cfg)?;
    println!("added `{name}` → {}", user_mcp_path().display());
    Ok(())
}

async fn cmd_remove(name: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = load_user_or_empty()?;
    if cfg.mcp_servers.remove(&name).is_none() {
        return Err(format!("server `{name}` not found in user config").into());
    }
    save_user_config(&cfg)?;
    println!("removed `{name}` from {}", user_mcp_path().display());
    Ok(())
}

async fn cmd_doctor(
    name: Option<String>,
    cwd: &std::path::Path,
    as_json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = one_mcp::load_effective(cwd)?;
    let cfg = &loaded.config;
    let targets: Vec<(String, McpServerConfig)> = if let Some(n) = name {
        let s = cfg
            .mcp_servers
            .get(&n)
            .cloned()
            .ok_or_else(|| format!("server `{n}` not configured"))?;
        vec![(n, s)]
    } else {
        cfg.mcp_servers
            .iter()
            .filter(|(_, s)| s.enabled.unwrap_or(true))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    if targets.is_empty() {
        println!("No enabled MCP servers to probe.");
        return Ok(());
    }

    let mut reports = Vec::new();
    for (n, s) in targets {
        eprint!("probing {n}... ");
        let h = probe_server(&n, &s).await;
        if h.ok {
            eprintln!("ok ({} tools)", h.tool_count);
        } else {
            eprintln!("FAIL");
        }
        reports.push(h);
    }

    if as_json {
        let v: Vec<serde_json::Value> = reports
            .iter()
            .map(|h| {
                let src = loaded
                    .server_sources
                    .get(&h.name)
                    .map(|k| k.as_str())
                    .unwrap_or("?");
                serde_json::json!({
                    "name": h.name,
                    "transport": h.transport,
                    "ok": h.ok,
                    "message": h.message,
                    "tool_count": h.tool_count,
                    "tools": h.tools,
                    "source": src,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    println!();
    for h in &reports {
        let status = if h.ok { "OK" } else { "FAIL" };
        let src = loaded
            .server_sources
            .get(&h.name)
            .map(|k| k.as_str())
            .unwrap_or("?");
        println!("[{status}] {} ({}, source={})", h.name, h.transport, src);
        println!("  {}", h.message);
        if !h.tools.is_empty() {
            println!("  tools: {}", h.tools.join(", "));
        }
    }
    let failed = reports.iter().filter(|h| !h.ok).count();
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(
            "server name must be non-empty and only [A-Za-z0-9_-]".into(),
        );
    }
    Ok(())
}

fn split_kv(pair: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (k, v) = pair
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got `{pair}`"))?;
    if k.is_empty() {
        return Err("empty env/header key".into());
    }
    Ok((k.to_string(), v.to_string()))
}

