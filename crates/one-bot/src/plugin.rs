//! Dynamic Connector Plugin Discovery and Lifecycle Manager.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

use crate::adapters::process::ProcessConnectorAdapter;
use crate::config::ConnectorConfig;
use crate::gateway::BotGateway;

/// Manager responsible for discovering and registering dynamic connector subprocesses.
pub struct ConnectorPluginManager;

impl ConnectorPluginManager {
    /// Register a connector from a typed `ConnectorConfig`.
    pub async fn register_connector_config(
        gateway: &BotGateway,
        config: &ConnectorConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !config.enabled {
            return Ok(());
        }

        let name = config.name.clone().unwrap_or_else(|| {
            PathBuf::from(&config.command)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("connector")
                .to_string()
        });

        info!(
            "Registering dynamic connector: {} (cmd: {})",
            name, config.command
        );
        let adapter = Arc::new(ProcessConnectorAdapter::new(
            config.command.clone(),
            config.args.clone(),
            Some(name),
        ));
        gateway.register_adapter(adapter).await;
        Ok(())
    }

    /// Discover and register all executable connectors found in a directory.
    pub async fn load_from_dir(gateway: &BotGateway, dir: impl AsRef<Path>) -> usize {
        let dir = dir.as_ref();
        if !dir.exists() || !dir.is_dir() {
            return 0;
        }

        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(meta) = path.metadata() {
                            if meta.permissions().mode() & 0o111 == 0 {
                                continue; // Not executable
                            }
                        }
                    }

                    let file_name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("connector");
                    info!("Discovered dynamic connector binary: {}", path.display());
                    let adapter = Arc::new(ProcessConnectorAdapter::new(
                        path.to_string_lossy().to_string(),
                        Vec::new(),
                        Some(file_name.to_string()),
                    ));
                    gateway.register_adapter(adapter).await;
                    count += 1;
                }
            }
        }
        count
    }

    /// Register a specific connector command or executable path.
    pub async fn register_connector_command(
        gateway: &BotGateway,
        command_line: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let parts: Vec<String> = shell_words(command_line);
        if parts.is_empty() {
            return Err("Empty connector command".into());
        }

        let cmd = parts[0].clone();
        let args = parts[1..].to_vec();

        let default_name = PathBuf::from(&cmd)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("connector")
            .to_string();

        info!("Registering dynamic connector: {} {:?}", cmd, args);
        let adapter = Arc::new(ProcessConnectorAdapter::new(cmd, args, Some(default_name)));
        gateway.register_adapter(adapter).await;
        Ok(())
    }
}

/// Simple shell argument splitter.
fn shell_words(input: &str) -> Vec<String> {
    input.split_whitespace().map(|s| s.to_string()).collect()
}
