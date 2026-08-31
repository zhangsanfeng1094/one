//! Interactive Hermes-style setup wizard for One Bot Gateway.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

/// Prompt the user with a question and an optional default value.
fn prompt_text(label: &str, default: Option<&str>, hint: Option<&str>) -> String {
    print!("{label}");
    if let Some(h) = hint {
        print!(" ({h})");
    }
    if let Some(d) = default {
        print!(" [{d}]");
    }
    print!(": ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            default.unwrap_or("").to_string()
        } else {
            trimmed.to_string()
        }
    } else {
        default.unwrap_or("").to_string()
    }
}

/// Prompt the user with a yes/no question.
fn prompt_bool(label: &str, default_yes: bool) -> bool {
    let def_str = if default_yes { "Y/n" } else { "y/N" };
    print!("{label} [{def_str}]: ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim().to_lowercase();
        if trimmed.is_empty() {
            default_yes
        } else {
            trimmed == "y" || trimmed == "yes" || trimmed == "true" || trimmed == "1"
        }
    } else {
        default_yes
    }
}

/// Run the interactive setup wizard and write out the generated configuration file.
/// Returns the path to the saved configuration file if created.
pub fn run_interactive_setup() -> Result<PathBuf, Box<dyn std::error::Error>> {
    println!("\n╔═══════════════════════════════════════════════════════════════════╗");
    println!("║       One Bot Gateway — Interactive Setup Wizard (Hermes)         ║");
    println!("╚═══════════════════════════════════════════════════════════════════╝\n");
    println!("This wizard will guide you through setting up multi-channel AI bot integration.");
    println!("Press Enter to accept defaults in [brackets].\n");

    // 1. LLM Model Selection
    println!("─── [1/4] LLM Model Configuration ───────────────────────────────────");
    let model = prompt_text(
        "Default LLM model",
        Some("grok-4"),
        Some("e.g. grok-4, claude-3-7-sonnet, gpt-4o, deepseek-chat"),
    );

    // 2. Platform Selection & Credentials
    println!("\n─── [2/4] Chat Platform Integrations ────────────────────────────────");

    // Telegram
    let enable_tg = prompt_bool("Enable Telegram Bot?", true);
    let mut tg_token = String::new();
    let mut tg_api_base = "https://api.telegram.org".to_string();
    if enable_tg {
        tg_token = prompt_text(
            "  Telegram Bot Token",
            None,
            Some("obtain from @BotFather, e.g. 123456:ABC-DEF..."),
        );
        let custom_base = prompt_text(
            "  Telegram API Base URL",
            Some("https://api.telegram.org"),
            Some("leave default unless using local Bot API proxy"),
        );
        if !custom_base.is_empty() {
            tg_api_base = custom_base;
        }
    }

    // Discord
    let enable_dc = prompt_bool("Enable Discord Bot?", false);
    let mut dc_token = String::new();
    let mut dc_app_id = String::new();
    if enable_dc {
        dc_token = prompt_text(
            "  Discord Bot Token",
            None,
            Some("from Discord Developer Portal -> Bot -> Token"),
        );
        dc_app_id = prompt_text(
            "  Discord Application ID",
            None,
            Some("from Discord Developer Portal -> General Information"),
        );
    }

    // Feishu / Lark
    let enable_feishu = prompt_bool("Enable Feishu / Lark (飞书) Bot?", false);
    let mut feishu_app_id = String::new();
    let mut feishu_app_secret = String::new();
    if enable_feishu {
        feishu_app_id = prompt_text(
            "  Feishu App ID (cli_xxx)",
            None,
            Some("from Feishu Open Platform -> App Details"),
        );
        feishu_app_secret = prompt_text(
            "  Feishu App Secret",
            None,
            Some("from Feishu Open Platform -> App Details"),
        );
    }

    // 3. Security & Permissions (HITL)
    println!("\n─── [3/4] Security & Human-in-the-Loop (HITL) ───────────────────────");
    let allowed_users_input = prompt_text(
        "Allowed User IDs",
        None,
        Some("comma-separated, e.g. tg_123456, dc_789; empty = open testing"),
    );
    let admin_users_input = prompt_text(
        "Admin User IDs",
        None,
        Some("comma-separated; allowed to run /model switch"),
    );
    let require_bash_approval = prompt_bool(
        "Require interactive button approval for Bash / dangerous tool operations?",
        true,
    );

    // 4. Save Destination
    println!("\n─── [4/4] Save Configuration ────────────────────────────────────────");
    println!("1. Local file in current directory (./bot.toml)");
    println!("2. Global user config (~/.one/bot.toml)");
    let save_choice = prompt_text("Select destination [1/2]", Some("1"), None);

    let target_path = if save_choice == "2" {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let one_dir = home.join(".one");
            let _ = fs::create_dir_all(&one_dir);
            one_dir.join("bot.toml")
        } else {
            PathBuf::from("bot.toml")
        }
    } else {
        PathBuf::from("bot.toml")
    };

    // Parse user lists
    let allowed_users: Vec<String> = allowed_users_input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let admin_users: Vec<String> = admin_users_input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Generate formatted TOML content
    let toml_content = generate_toml_content(
        &model,
        &allowed_users,
        &admin_users,
        require_bash_approval,
        enable_tg,
        &tg_token,
        &tg_api_base,
        enable_dc,
        &dc_token,
        &dc_app_id,
        enable_feishu,
        &feishu_app_id,
        &feishu_app_secret,
    );

    if target_path.exists() {
        let overwrite = prompt_bool(
            &format!(
                "Target file '{}' already exists. Overwrite?",
                target_path.display()
            ),
            true,
        );
        if !overwrite {
            println!("Setup cancelled. Existing file was preserved.");
            return Ok(target_path);
        }
    }

    fs::write(&target_path, toml_content)?;

    println!("\n╔═══════════════════════════════════════════════════════════════════╗");
    println!(
        "║  ✓ Configuration successfully saved to: {:<25} ║",
        target_path.display()
    );
    println!("╚═══════════════════════════════════════════════════════════════════╝\n");
    println!("Summary:");
    println!("  • Model: {}", model);
    println!(
        "  • Telegram: {}",
        if enable_tg { "Enabled" } else { "Disabled" }
    );
    println!(
        "  • Discord:  {}",
        if enable_dc { "Enabled" } else { "Disabled" }
    );
    println!(
        "  • Feishu:   {}",
        if enable_feishu { "Enabled" } else { "Disabled" }
    );
    println!(
        "  • Bash Approval: {}",
        if require_bash_approval {
            "Yes (HITL Buttons)"
        } else {
            "Auto-Approve"
        }
    );
    println!("\nYou can launch the bot at any time with:");
    println!("  one bot -C {}\n", target_path.display());

    Ok(target_path)
}

/// Helper to format the generated TOML string with clean comments and layout.
fn generate_toml_content(
    model: &str,
    allowed_users: &[String],
    admin_users: &[String],
    require_bash_approval: bool,
    enable_tg: bool,
    tg_token: &str,
    tg_api_base: &str,
    enable_dc: bool,
    dc_token: &str,
    dc_app_id: &str,
    enable_feishu: bool,
    feishu_app_id: &str,
    feishu_app_secret: &str,
) -> String {
    let allowed_str = serde_json::to_string(allowed_users).unwrap_or_else(|_| "[]".to_string());
    let admin_str = serde_json::to_string(admin_users).unwrap_or_else(|_| "[]".to_string());

    format!(
        r#"# ==============================================================================
# One Bot Gateway Configuration (Hermes-aligned Auto Config)
# ==============================================================================

# Default LLM model for bot conversations
default_model = "{model}"

# Maximum concurrent sessions
max_concurrent_sessions = 50

# ------------------------------------------------------------------------------
# Security & HITL (Human-in-the-Loop) Configuration
# ------------------------------------------------------------------------------
[security]
allowed_users = {allowed_str}
admin_users = {admin_str}
allowed_channels = []
require_approval_for_bash = {require_bash_approval}

# ------------------------------------------------------------------------------
# Native Telegram Adapter
# ------------------------------------------------------------------------------
[telegram]
enabled = {enable_tg}
bot_token = "{tg_token}"
api_base = "{tg_api_base}"
stream_edit_interval_ms = 1000

# ------------------------------------------------------------------------------
# Native Discord Adapter
# ------------------------------------------------------------------------------
[discord]
enabled = {enable_dc}
bot_token = "{dc_token}"
application_id = "{dc_app_id}"

# ------------------------------------------------------------------------------
# Native Feishu / Lark Adapter
# ------------------------------------------------------------------------------
[feishu]
enabled = {enable_feishu}
app_id = "{feishu_app_id}"
app_secret = "{feishu_app_secret}"

# ------------------------------------------------------------------------------
# Slack
# ------------------------------------------------------------------------------
# Native Slack support is not implemented. Configure Slack through an external
# subprocess connector instead.

# ------------------------------------------------------------------------------
# External Dynamic Subprocess Connectors (JSON-RPC stdio)
# ------------------------------------------------------------------------------
# [[connectors]]
# name = "wecom"
# command = "python3"
# args = ["./connectors/wecom.py"]
# enabled = true
"#
    )
}

/// Check whether the terminal is interactive and no adapters are configured.
pub fn should_offer_interactive_setup(config: &one_bot::BotConfig) -> bool {
    let has_any_active = (config.telegram.enabled && !config.telegram.bot_token.is_empty())
        || (config.discord.enabled && !config.discord.bot_token.is_empty())
        || (config.feishu.enabled && !config.feishu.app_id.is_empty())
        || !config.connectors.is_empty();

    !has_any_active && io::stdin().is_terminal()
}
