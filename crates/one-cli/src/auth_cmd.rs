//! `one login` / `one logout` CLI.

use std::io::{self, Write};

use one_ai::auth::{
    login_openai_codex_browser_persist, login_openai_codex_device_persist, login_provider,
    login_xai_browser_persist, login_xai_device_persist, oauth_provider_catalog, AuthEvent,
    AuthInteraction, AuthPrompt, AuthStorage, PROVIDER_OPENAI_CODEX, PROVIDER_OPENCODE,
    PROVIDER_OPENCODE_GO, PROVIDER_XAI,
};

use crate::cli::{LoginCli, LogoutCli};

/// Stdio interaction for CLI login (prints URLs / codes, reads paste from stdin).
struct CliAuthInteraction {
    cancelled: bool,
}

impl CliAuthInteraction {
    fn new() -> Self {
        Self { cancelled: false }
    }
}

#[async_trait::async_trait]
impl AuthInteraction for CliAuthInteraction {
    fn notify(&mut self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                eprintln!();
                eprintln!("  Open this URL to sign in:");
                eprintln!("  {url}");
                if let Some(ins) = instructions {
                    eprintln!();
                    eprintln!("  {ins}");
                }
                eprintln!();
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                expires_in_seconds,
                ..
            } => {
                eprintln!();
                eprintln!("  Visit: {verification_uri}");
                eprintln!("  Enter code: {user_code}");
                if let Some(secs) = expires_in_seconds {
                    eprintln!("  (expires in ~{} min)", secs / 60);
                }
                eprintln!();
            }
            AuthEvent::Progress { message } => {
                eprintln!("  … {message}");
            }
            AuthEvent::Info { message } => {
                eprintln!("  {message}");
            }
        }
    }

    async fn prompt(&mut self, prompt: AuthPrompt) -> Result<String, String> {
        match prompt {
            AuthPrompt::Select { message, options } => {
                eprintln!("{message}");
                for (i, opt) in options.iter().enumerate() {
                    eprintln!("  [{}] {}", i + 1, opt.label);
                }
                eprint!("Choice [1]: ");
                let _ = io::stderr().flush();
                let line = read_stdin_line().await?;
                let line = line.trim();
                if line.is_empty() {
                    return Ok(options
                        .first()
                        .map(|o| o.id.clone())
                        .unwrap_or_default());
                }
                if let Ok(n) = line.parse::<usize>() {
                    if let Some(opt) = options.get(n.saturating_sub(1)) {
                        return Ok(opt.id.clone());
                    }
                }
                if options.iter().any(|o| o.id == line) {
                    return Ok(line.to_string());
                }
                Err(format!("invalid choice: {line}"))
            }
            AuthPrompt::ManualCode {
                message,
                placeholder,
            }
            | AuthPrompt::Text {
                message,
                placeholder,
            } => {
                eprintln!("{message}");
                if let Some(ph) = placeholder {
                    eprintln!("  (e.g. {ph})");
                }
                eprint!("> ");
                let _ = io::stderr().flush();
                // Race against OAuth callback: empty line is ignored by select! path
                // only if user pastes a code. Hitting Enter without a code cancels manual
                // side so the browser callback (if any) can still win via select! — but
                // select! completes when either finishes. So we park until non-empty or EOF.
                loop {
                    let line = read_stdin_line().await?;
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        return Ok(line);
                    }
                    eprintln!("  (waiting for browser callback, or paste code and press Enter)");
                    eprint!("> ");
                    let _ = io::stderr().flush();
                }
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// Run interactive / CLI login. Returns the resolved provider id that was used.
pub async fn run_login(cli: LoginCli) -> Result<String, Box<dyn std::error::Error>> {
    let storage = AuthStorage::create()?;
    let mut ix = CliAuthInteraction::new();

    let provider = match cli.provider.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => normalize_provider(raw),
        None => {
            // No provider on argv → pick from catalog (Codex / OpenCode Zen / Go …).
            pick_login_provider(&mut ix).await?
        }
    };

    eprintln!("Logging in to `{provider}` …");
    eprintln!("Credentials → {}", storage.path().display());

    // --browser / --device-code skip method select for real OAuth providers.
    if cli.device_code || cli.browser {
        let (pid, cred) = match provider.as_str() {
            PROVIDER_OPENAI_CODEX => {
                let cred = if cli.device_code {
                    login_openai_codex_device_persist(&storage, &mut ix).await?
                } else {
                    login_openai_codex_browser_persist(&storage, &mut ix).await?
                };
                (PROVIDER_OPENAI_CODEX, cred)
            }
            PROVIDER_XAI => {
                let cred = if cli.device_code {
                    login_xai_device_persist(&storage, &mut ix).await?
                } else {
                    login_xai_browser_persist(&storage, &mut ix).await?
                };
                (PROVIDER_XAI, cred)
            }
            other => {
                return Err(format!(
                    "--browser / --device-code only apply to `{PROVIDER_OPENAI_CODEX}` or `{PROVIDER_XAI}` \
                     (got `{other}`)"
                )
                .into());
            }
        };
        let account = cred.account_id.as_deref().unwrap_or("(oauth)");
        eprintln!();
        eprintln!("✓ Logged in as {pid} ({account})");
        eprintln!("  access token expires at epoch_ms={}", cred.expires);
        seed_after_login(pid)?;
        return Ok(pid.into());
    }

    let cred = login_provider(&storage, &provider, &mut ix).await?;

    match &cred {
        one_ai::AuthCredential::OAuth(o) => {
            let account = o.account_id.as_deref().unwrap_or("(unknown account)");
            eprintln!();
            eprintln!("✓ Logged in as `{provider}` (account {account})");
            eprintln!("  access token expires at epoch_ms={}", o.expires);
        }
        one_ai::AuthCredential::ApiKey(_) => {
            eprintln!();
            eprintln!("✓ Logged in as `{provider}` (API key stored)");
        }
    }

    seed_after_login(&provider)?;
    Ok(provider)
}

/// Interactive catalog picker (used when `one login` / `/login` omit the provider).
async fn pick_login_provider(
    ix: &mut dyn AuthInteraction,
) -> Result<String, Box<dyn std::error::Error>> {
    use one_ai::auth::SelectOption;

    let catalog = oauth_provider_catalog();
    let options: Vec<SelectOption> = catalog
        .iter()
        .map(|p| SelectOption {
            id: p.id.into(),
            label: format!("{} — {}", p.name, p.description),
        })
        .collect();

    let chosen = ix
        .prompt(AuthPrompt::Select {
            message: "Select a provider to log in:".into(),
            options,
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    Ok(normalize_provider(&chosen))
}

fn seed_after_login(provider: &str) -> Result<(), Box<dyn std::error::Error>> {
    match provider {
        PROVIDER_OPENAI_CODEX => match one_ai::seed_openai_codex_models_default() {
            Ok(report) => {
                eprintln!();
                eprintln!(
                    "✓ models.json ← {} Codex models ({} new, {} updated)",
                    report.total, report.added, report.updated
                );
                eprintln!("  {}", report.path.display());
                eprintln!("  default: openai-codex:{}", report.default_model);
                for m in one_ai::OPENAI_CODEX_BUILTIN_MODELS {
                    eprintln!("    · {}:{}", one_ai::PROVIDER_OPENAI_CODEX, m.id);
                }
                eprintln!();
                eprintln!(
                    "Use: one --provider openai-codex --model {} -p \"hello\"",
                    report.default_model
                );
                eprintln!(
                    "  or: /model openai-codex:{}  inside the TUI",
                    report.default_model
                );
            }
            Err(e) => {
                eprintln!("! failed to seed models.json: {e}");
                eprintln!("  (login ok — add models manually or fix models.json permissions)");
            }
        },
        PROVIDER_XAI => match one_ai::seed_xai_models_default() {
            Ok(report) => {
                eprintln!();
                eprintln!(
                    "✓ models.json ← {} Grok models ({} new, {} updated)",
                    report.total, report.added, report.updated
                );
                eprintln!("  {}", report.path.display());
                eprintln!("  default: xai:{}", report.default_model);
                for m in one_ai::XAI_BUILTIN_MODELS {
                    eprintln!("    · {}:{}", one_ai::PROVIDER_XAI, m.id);
                }
                eprintln!();
                eprintln!(
                    "Use: one --provider xai --model {} -p \"hello\"",
                    report.default_model
                );
                eprintln!(
                    "  or: /model xai:{}  inside the TUI",
                    report.default_model
                );
            }
            Err(e) => {
                eprintln!("! failed to seed models.json: {e}");
                eprintln!("  (login ok — add models manually or fix models.json permissions)");
            }
        },
        PROVIDER_OPENCODE | PROVIDER_OPENCODE_GO => {
            // Seed both catalogs so one key unlocks Zen + Go.
            match one_ai::seed_opencode_models_default("both") {
                Ok(report) => {
                    eprintln!();
                    eprintln!(
                        "✓ models.json ← {} OpenCode models ({} new, {} updated)",
                        report.total, report.added, report.updated
                    );
                    eprintln!("  {}", report.path.display());
                    eprintln!("  providers: {}", report.provider);
                    let def = if provider == PROVIDER_OPENCODE {
                        one_ai::OPENCODE_ZEN_DEFAULT_MODEL
                    } else {
                        one_ai::OPENCODE_GO_DEFAULT_MODEL
                    };
                    eprintln!("  default for `{provider}`: {provider}:{def}");
                    eprintln!();
                    eprintln!("Use: one --provider {provider} --model {def} -p \"hello\"");
                    eprintln!("  or: /model {provider}:{def}  inside the TUI");
                }
                Err(e) => {
                    eprintln!("! failed to seed models.json: {e}");
                    eprintln!("  (login ok — add models manually or fix models.json permissions)");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub async fn run_logout(cli: LogoutCli) -> Result<(), Box<dyn std::error::Error>> {
    let storage = AuthStorage::create()?;
    if cli.all {
        let providers = storage.list();
        if providers.is_empty() {
            eprintln!("No stored credentials.");
            return Ok(());
        }
        for p in providers {
            storage.logout(&p)?;
            eprintln!("Logged out `{p}`");
        }
        return Ok(());
    }
    let provider = normalize_provider(&cli.provider);
    if storage.logout(&provider)? {
        eprintln!("Logged out `{provider}`");
    } else {
        eprintln!("No stored credential for `{provider}`");
    }
    Ok(())
}

fn normalize_provider(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "codex" | "chatgpt" | "openai-codex" | "openai_codex" => PROVIDER_OPENAI_CODEX.into(),
        "zen" | "opencode-zen" | "opencode_zen" | "opencode" => PROVIDER_OPENCODE.into(),
        "go" | "opencode_go" | "opencode-go" => PROVIDER_OPENCODE_GO.into(),
        "grok" | "xai" | "xai-oauth" | "supergrok" | "xai_oauth" => PROVIDER_XAI.into(),
        other => other.to_string(),
    }
}

async fn read_stdin_line() -> Result<String, String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        Ok(line)
    })
    .await
    .map_err(|e| e.to_string())?
}

pub fn print_oauth_providers() {
    eprintln!("Login providers (subscription / OAuth):");
    for p in oauth_provider_catalog() {
        eprintln!("  {:<16} {}", p.id, p.name);
        eprintln!("                   {}", p.description);
    }
}
