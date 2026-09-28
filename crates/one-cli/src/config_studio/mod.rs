//! One Config Studio: a local configuration console for the One agent.
//!
//! Implements the P0 + P1 scope of `docs/web-config.md`:
//!
//! * **P0 — configuration facts and shared boundaries.** [`adapters`] inventories
//!   every configuration source the runtime reads, reports real paths and
//!   precedence, and validates drafts with the runtime's own loaders so the
//!   studio can never drift from actual behaviour.
//! * **P1 — usable first release.** [`api`] serves the catalog, document views,
//!   draft validation, masked diffs, transactional saves, backups, restore, and
//!   conflict detection; [`security`] confines the server to loopback with a
//!   token, Host allowlist, and Origin check.
//!
//! Two ideas are kept strictly separate, per the spec:
//!
//! * **编辑目标** — a concrete file in a concrete layer ([`document::ConfigDocument`]).
//! * **生效配置** — the merged result the runtime will use
//!   ([`document::EffectiveReport`]), always read-only and source-attributed.
//!
//! The studio never claims to represent other running sessions: every preview is
//! computed against this process's own context ([`document::StudioContext`]).

pub mod adapters;
pub mod api;
pub mod diff;
pub mod document;
mod enhancers;
pub mod mask;
pub mod save;
pub mod security;

use std::error::Error;
use std::path::PathBuf;
use std::rc::Rc;

pub use api::ConfigStudio;
pub use save::BackupStore;
pub use security::StudioSecurity;

use one_web::{start_web_server, LocalAcpHandler, WebServerConfig};

/// Default port for the config studio.
pub const DEFAULT_PORT: u16 = 3333;

/// Startup parameters for the studio server.
#[derive(Debug, Clone)]
pub struct StudioServerConfig {
    /// Bind host; must be a loopback address.
    pub host: String,
    /// Bind port; `0` is rejected because the startup URL embeds the port.
    pub port: u16,
    /// Open the browser at the tokenized URL once bound.
    pub open_browser: bool,
    /// Working directory whose project layers are inspected.
    pub cwd: PathBuf,
}

impl StudioServerConfig {
    /// Configuration for `cwd` on the default host and port.
    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            open_browser: false,
            cwd,
        }
    }
}

/// Run the studio server until interrupted.
///
/// Deliberately does not build a provider or check authentication: the config
/// center must be reachable precisely when the configuration is broken.
pub async fn run(config: StudioServerConfig) -> Result<(), Box<dyn Error>> {
    if !security::is_loopback_host(&config.host) {
        return Err(format!(
            "拒绝启动配置中心：只能绑定回环地址（127.0.0.1 / ::1 / localhost），收到 `{}`。\
             配置中心可读写凭据与配置文件，绝不能暴露到网络。",
            config.host
        )
        .into());
    }
    if config.port == 0 {
        return Err("配置中心需要一个固定端口，以便把访问凭证写进启动链接（请指定 --port）".into());
    }

    let studio = Rc::new(ConfigStudio::new(config.cwd.clone()));
    let security = Rc::new(StudioSecurity::new(&config.host, config.port));
    let url = format!(
        "http://{}:{}/config?token={}",
        config.host,
        config.port,
        security.token()
    );

    let banner = render_banner(&studio, &url);

    let server_config = WebServerConfig {
        host: config.host.clone(),
        port: config.port,
        open_browser: config.open_browser,
        info_json: serde_json::json!({
            "name": "one",
            "version": env!("CARGO_PKG_VERSION"),
            "mode": "config",
            "cwd": studio.paths().cwd.display().to_string(),
            "agent_dir": studio.paths().agent_dir.display().to_string(),
        }),
        banner: Some(banner),
        // Same-origin only: the config API must never be reachable cross-origin.
        cors_allow_origin: None,
        guard: Some(security.guard()),
        api: Some(studio.api_handler()),
    };

    // The config studio serves no chat, so any ACP upgrade is a no-op.
    let acp_factory = || {
        let handler: LocalAcpHandler = Rc::new(|_incoming, _outgoing| Box::pin(async {}));
        handler
    };

    start_web_server(server_config, acp_factory).await
}

/// Startup banner, including the tokenized URL and the effect-timing promise.
///
/// The bound address is already inside `url`, so the banner needs no other
/// server setting.
fn render_banner(studio: &ConfigStudio, url: &str) -> String {
    let context = studio.context();
    let writable: Vec<String> = studio
        .documents()
        .into_iter()
        .filter(|d| d.writable())
        .map(|d| d.doc.id)
        .collect();

    let mut out = String::new();
    out.push_str("╔══════════════════════════════════════════════════════════════════╗\n");
    out.push_str("║        ⚙  One Config Studio — 本地配置控制台                     ║\n");
    out.push_str("╠══════════════════════════════════════════════════════════════════╣\n");
    out.push_str(&format!("║  URL:    {url:<56} ║\n"));
    out.push_str("╠══════════════════════════════════════════════════════════════════╣\n");
    out.push_str(&format!(
        "║  cwd:      {:<52} ║\n",
        truncate(&context.cwd, 52)
    ));
    out.push_str(&format!(
        "║  agent:    {:<52} ║\n",
        truncate(&context.agent_dir, 52)
    ));
    out.push_str(&format!(
        "║  可写文档: {:<52} ║\n",
        truncate(&writable.join(", "), 52)
    ));
    out.push_str("╠══════════════════════════════════════════════════════════════════╣\n");
    out.push_str("║  仅监听回环地址 · Host/Origin 校验 · 访问需携带临时令牌          ║\n");
    out.push_str("║  保存为单文档事务：校验 → 备份 → 临时文件 → 原子替换            ║\n");
    out.push_str("║  默认“新会话生效”，不会重启正在运行的会话或 MCP 子进程          ║\n");
    out.push_str("╚══════════════════════════════════════════════════════════════════╝\n");
    out.push_str(&format!(
        "Backups: {}\n",
        BackupStore::new(&studio.paths().agent_dir).root().display()
    ));
    out
}

/// Shorten a string for the fixed-width banner.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_host() {
        let cfg = StudioServerConfig {
            host: "0.0.0.0".to_string(),
            port: 3333,
            open_browser: false,
            cwd: PathBuf::from("/tmp"),
        };
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(cfg))
            .unwrap_err();
        assert!(err.to_string().contains("回环"));
    }

    #[test]
    fn rejects_ephemeral_port() {
        let cfg = StudioServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            open_browser: false,
            cwd: PathBuf::from("/tmp"),
        };
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(cfg))
            .unwrap_err();
        assert!(err.to_string().contains("固定端口"));
    }

    #[test]
    fn banner_mentions_the_effect_promise_and_token() {
        let root = std::env::temp_dir().join(format!("one-config-banner-{}", std::process::id()));
        let studio = ConfigStudio::with_paths(adapters::StudioPaths {
            cwd: root.clone(),
            agent_dir: root.join("agent"),
            home: root.clone(),
            project_chain: vec![root.clone()],
        });
        let banner = render_banner(&studio, "http://127.0.0.1:3333/config?token=abc");

        assert!(banner.contains("token=abc"));
        assert!(banner.contains("新会话生效"));
        assert!(banner.contains("原子替换"));
        assert!(banner.contains("settings.global"));
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdefghij", 5).chars().count(), 5);
    }
}
