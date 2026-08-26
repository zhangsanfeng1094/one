//! Unified settings at `~/.one/agent/settings.json`.
//!
//! Migrates from legacy `preferences.json` (provider + model only).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::preferences;

/// Per-skill enable/disable (Codex `[[skills.config]]` equivalent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConfigEntry {
    /// Absolute path to `SKILL.md`.
    pub path: String,
    /// When false, skill is hidden from catalog and not force-loadable.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// OpenCode-style tool output caps (settings key `tool_output`).
///
/// Defaults when omitted: 2000 lines / 50 KiB. Over either limit → full spill
/// under `~/.one/agent/tool-outputs/` + preview + path for the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputSettings {
    /// Max lines kept inline before spill (default 2000).
    pub max_lines: Option<usize>,
    /// Max UTF-8 bytes kept inline before spill (default 51200).
    pub max_bytes: Option<usize>,
}

/// Context compaction strategy (settings key `compaction`).
///
/// Main path: auto threshold + keep_recent **user turns** verbatim after compact.
/// `prune` (default **on**): every turn, trim old tool bodies by user-turn age.
/// `two_pass` (default **off**): Pass-1 NOTE₁ + Pass-2 final; background prefire
/// starts `prefire_lead_ratio` of the window below the auto-compact limit.
/// Omitted fields use defaults in [`CompactionSettings::to_config`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionSettings {
    /// Auto-compact when over threshold before a turn (default true).
    pub auto: Option<bool>,
    /// Fraction of context_window that triggers compact (0.0–1.0, default 0.85).
    /// Ignored when [`Self::threshold`] is set.
    pub ratio: Option<f64>,
    /// Absolute token threshold override (takes precedence over ratio).
    pub threshold: Option<usize>,
    /// Recent user turns kept verbatim after compact (default 2).
    pub keep_recent: Option<usize>,
    /// Prune old tool bodies by user-turn age every compact check (default true).
    pub prune: Option<bool>,
    /// Legacy: unused by turn-age prune (kept for settings.json compat).
    pub prune_protect_tokens: Option<usize>,
    /// Legacy: unused by turn-age prune (kept for settings.json compat).
    pub prune_max_chars: Option<usize>,
    /// Recent user turns whose tool results are never pruned (default 3).
    pub prune_keep_last_n_turns: Option<usize>,
    /// Char threshold for soft-trim of older tool results (default 4000).
    pub prune_soft_trim_threshold: Option<usize>,
    pub prune_soft_trim_head: Option<usize>,
    pub prune_soft_trim_tail: Option<usize>,
    /// User-turn age after which tool results become a placeholder (default 10).
    pub prune_hard_clear_age_turns: Option<usize>,
    /// Opt-in two-pass summarization + background Pass-1 prefire (default false).
    pub two_pass: Option<bool>,
    /// Fraction of context_window below the auto-compact limit at which Pass-1
    /// prefires (default 0.10). Grok `GROK_PREFIRE_LEAD_PERCENT`.
    pub prefire_lead_ratio: Option<f64>,
    /// Deprecated: fraction of compact threshold. Converted to lead if lead unset.
    pub prefire_ratio: Option<f64>,
}

impl CompactionSettings {
    /// Resolve into a runtime [`one_core::CompactionConfig`] for `context_window`.
    pub fn to_config(&self, context_window: usize) -> one_core::CompactionConfig {
        let ratio = self
            .ratio
            .filter(|r| r.is_finite() && *r > 0.0 && *r <= 1.0)
            .unwrap_or(one_core::DEFAULT_COMPACT_RATIO);
        let mut cfg = one_core::CompactionConfig::from_window_and_ratio(context_window, ratio);
        cfg.enabled = self.auto.unwrap_or(true);
        if let Some(n) = self.threshold.filter(|n| *n > 0) {
            cfg.token_threshold = n;
        }
        if let Some(n) = self.keep_recent.filter(|n| *n > 0) {
            cfg.keep_recent_messages = n;
        }
        cfg.prune = self.prune.unwrap_or(true);
        if let Some(n) = self.prune_protect_tokens {
            cfg.prune_protect_tokens = n;
        }
        if let Some(n) = self.prune_max_chars {
            cfg.prune_max_chars = n;
        }
        if let Some(n) = self.prune_keep_last_n_turns.filter(|n| *n > 0) {
            cfg.prune_keep_last_n_turns = n;
        }
        if let Some(n) = self.prune_soft_trim_threshold {
            cfg.prune_soft_trim_threshold = n;
        }
        if let Some(n) = self.prune_soft_trim_head {
            cfg.prune_soft_trim_head = n;
        }
        if let Some(n) = self.prune_soft_trim_tail {
            cfg.prune_soft_trim_tail = n;
        }
        if let Some(n) = self.prune_hard_clear_age_turns.filter(|n| *n > 0) {
            cfg.prune_hard_clear_age_turns = n;
        }
        cfg.two_pass = self.two_pass.unwrap_or(false);
        if let Some(lead) = self
            .prefire_lead_ratio
            .filter(|r| r.is_finite() && *r > 0.0 && *r < 1.0)
        {
            cfg.prefire_lead_ratio = lead;
        } else if let Some(r) = self
            .prefire_ratio
            .filter(|r| r.is_finite() && *r > 0.0 && *r < 1.0)
        {
            // Old meaning: fire at r × threshold. Approximate as lead = 1 − r.
            cfg.prefire_lead_ratio = (1.0 - r).clamp(0.01, 0.49);
            cfg.prefire_ratio = r;
        }
        cfg
    }

    /// One-line summary for Settings UI, e.g. `auto 85% · keep 2 · prune · 2-pass`.
    pub fn summary_line(&self) -> String {
        let auto = if self.auto.unwrap_or(true) {
            "auto"
        } else {
            "manual"
        };
        let thresh = if let Some(n) = self.threshold.filter(|n| *n > 0) {
            if n >= 1000 {
                format!("{}k", n / 1000)
            } else {
                n.to_string()
            }
        } else {
            let r = self
                .ratio
                .filter(|r| r.is_finite() && *r > 0.0 && *r <= 1.0)
                .unwrap_or(one_core::DEFAULT_COMPACT_RATIO);
            format!("{}%", (r * 100.0).round() as u32)
        };
        let keep = self
            .keep_recent
            .unwrap_or(one_core::DEFAULT_KEEP_RECENT_TURNS);
        let prune = if self.prune.unwrap_or(true) {
            "prune"
        } else {
            "no prune"
        };
        let two = if self.two_pass.unwrap_or(false) {
            let lead = self
                .prefire_lead_ratio
                .filter(|r| r.is_finite() && *r > 0.0 && *r < 1.0)
                .unwrap_or(one_core::DEFAULT_PREFIRE_LEAD_RATIO);
            format!(" · 2-pass lead {}%", (lead * 100.0).round() as u32)
        } else {
            String::new()
        };
        format!("{auto} {thresh} · keep {keep} · {prune}{two}")
    }
}

/// Cross-session memory L2 index (see `docs/memory.md`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemorySettings {
    /// Inject L2 catalog into system prompt (default true). Bodies still need `read`.
    pub enabled: Option<bool>,
    /// Max index entries in the catalog (default 80).
    pub index_max_lines: Option<usize>,
    /// Allow `write`/`edit` under memory roots (default true = agent write path).
    pub write: Option<bool>,
    /// Max memory `read`/`grep` ops per user turn (default 6).
    pub max_lookups_per_turn: Option<usize>,
    /// Default `AgentSpec.resources.memory` for **subagents** when not set on the
    /// child spec: `off` (default) | `index` (M4).
    pub subagent: Option<String>,
    /// Write compaction summaries under `memory/sessions/` (L4 archive, M5).
    /// Default true when memory is enabled.
    pub archive_compaction: Option<bool>,
}

impl MemorySettings {
    pub fn to_load_options(&self) -> one_resources::MemoryLoadOptions {
        one_resources::MemoryLoadOptions {
            enabled: self.enabled.unwrap_or(true),
            index_max_lines: self
                .index_max_lines
                .filter(|n| *n > 0)
                .unwrap_or(one_resources::DEFAULT_INDEX_MAX_LINES),
            write_enabled: self.write.unwrap_or(true),
            max_lookups_per_turn: self
                .max_lookups_per_turn
                .filter(|n| *n > 0)
                .unwrap_or(one_resources::DEFAULT_MAX_LOOKUPS_PER_TURN),
        }
    }

    /// Subagent memory mode (default off).
    pub fn subagent_mode(&self) -> crate::protocol::MemoryResourceMode {
        self.subagent
            .as_deref()
            .and_then(crate::protocol::MemoryResourceMode::parse)
            .unwrap_or(crate::protocol::MemoryResourceMode::Off)
    }

    pub fn archive_compaction_enabled(&self) -> bool {
        self.archive_compaction.unwrap_or(true) && self.enabled.unwrap_or(true)
    }

    pub fn summary_line(&self) -> String {
        if self.enabled.unwrap_or(true) {
            let n = self
                .index_max_lines
                .filter(|n| *n > 0)
                .unwrap_or(one_resources::DEFAULT_INDEX_MAX_LINES);
            let w = if self.write.unwrap_or(true) {
                "write on"
            } else {
                "write off"
            };
            let lookups = self
                .max_lookups_per_turn
                .filter(|n| *n > 0)
                .unwrap_or(one_resources::DEFAULT_MAX_LOOKUPS_PER_TURN);
            let sub = self.subagent_mode().as_str();
            let arch = if self.archive_compaction_enabled() {
                "archive on"
            } else {
                "archive off"
            };
            format!("L2 on · max {n} · {w} · lookups/{lookups} · sub={sub} · {arch}")
        } else {
            "off".into()
        }
    }
}

/// User settings — single source for durable interactive preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// off | low | medium | high
    pub thinking: Option<String>,
    /// Skip bash danger prompts.
    pub auto_approve: Option<bool>,
    /// Standardized permission mode: default / ask, acceptEdits, auto, dontAsk, bypassPermissions (always-approve).
    #[serde(
        default,
        rename = "permissionMode",
        alias = "permission_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<String>,
    /// Optional context window override for footer %.
    pub context_window: Option<usize>,
    /// Path sandbox: `workspace-write` (default) | `full-access`.
    pub sandbox: Option<String>,
    /// Extra directories the agent may read/write (same as `--add-dir`).
    pub additional_directories: Option<Vec<String>>,
    /// Fine-grained tool permission rules (Claude-style allow/deny/ask).
    pub permissions: Option<one_tools::PermissionRules>,
    /// Run bash under bubblewrap when sandbox is workspace-write (default true).
    pub bash_sandbox: Option<bool>,
    /// Skills enable/disable list (like Codex `[[skills.config]]`).
    /// Omitted paths default to enabled.
    pub skills_config: Option<Vec<SkillConfigEntry>>,
    /// Runtime feature flags (id → enabled). Omitted ids use registry defaults.
    /// See `runtime/features.rs` (e.g. `subagent` → task tools; `memory` → whole memory package).
    pub features: Option<HashMap<String, bool>>,
    /// Unified tool-output truncation (OpenCode `tool_output`).
    pub tool_output: Option<ToolOutputSettings>,
    /// Context compaction strategy (threshold + optional tool prune).
    pub compaction: Option<CompactionSettings>,
    /// Cross-session memory L2 catalog.
    pub memory: Option<MemorySettings>,
    /// Extra LLM samples after a blank turn or temporary provider failure.
    ///
    /// Default: [`one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES`] (10).
    /// `0` disables automatic retries. Override with env
    /// `ONE_EMPTY_RESPONSE_RETRIES` when set.
    pub empty_response_retries: Option<usize>,
    /// Models shown in the Ctrl+L / `/model` switcher (`provider:id` specs).
    ///
    /// Missing / empty / omitted → show **all** catalog models (no filter).
    /// When set, only these specs appear (current model is always included).
    #[serde(
        default,
        rename = "enabledModels",
        alias = "enabled_models",
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled_models: Option<Vec<String>>,
}

impl Settings {
    /// Effective compaction config for the active context window.
    pub fn compaction_config(&self, context_window: usize) -> one_core::CompactionConfig {
        self.compaction
            .as_ref()
            .map(|c| c.to_config(context_window))
            .unwrap_or_else(|| one_core::CompactionConfig::from_context_window(context_window))
    }

    /// Memory L2 load options (settings only — CLI/env overrides applied by caller).
    pub fn memory_load_options(&self) -> one_resources::MemoryLoadOptions {
        self.memory
            .as_ref()
            .map(|m| m.to_load_options())
            .unwrap_or_default()
    }

    pub fn memory_or_default(&self) -> MemorySettings {
        self.memory.clone().unwrap_or_default()
    }

    pub fn memory_mut(&mut self) -> &mut MemorySettings {
        if self.memory.is_none() {
            self.memory = Some(MemorySettings::default());
        }
        self.memory.as_mut().unwrap()
    }

    /// Retry budget for blank completions and temporary provider failures.
    pub fn empty_response_retries(&self) -> usize {
        if let Ok(v) = std::env::var("ONE_EMPTY_RESPONSE_RETRIES") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return n;
            }
        }
        self.empty_response_retries
            .unwrap_or(one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES)
    }

    pub fn compaction_or_default(&self) -> CompactionSettings {
        self.compaction.clone().unwrap_or_default()
    }

    fn compaction_mut(&mut self) -> &mut CompactionSettings {
        if self.compaction.is_none() {
            self.compaction = Some(CompactionSettings::default());
        }
        self.compaction.as_mut().expect("just set")
    }

    /// Apply `tool_output` (+ env overrides) to the process-wide truncate limits.
    pub fn apply_tool_output_limits(&self) {
        let (lines, bytes) = self
            .tool_output
            .as_ref()
            .map(|t| (t.max_lines, t.max_bytes))
            .unwrap_or((None, None));
        let lim = one_tools::ToolOutputLimits::from_env_and_overrides(lines, bytes);
        one_tools::set_tool_output_limits(lim);
    }

    /// Effective feature value (registry default when omitted).
    pub fn feature_enabled(&self, id: &str, default: bool) -> bool {
        self.features
            .as_ref()
            .and_then(|m| m.get(id))
            .copied()
            .unwrap_or(default)
    }

    /// Persist a feature flag (creates the map if needed).
    pub fn set_feature(&mut self, id: &str, enabled: bool) {
        // Normalize legacy `memory_write` → package id `memory`.
        let id = if id == "memory_write" { "memory" } else { id };
        let map = self.features.get_or_insert_with(HashMap::new);
        map.insert(id.to_string(), enabled);
        // Drop legacy key if present so fingerprint stays clean.
        if id == "memory" {
            map.remove("memory_write");
        }
    }

    pub fn skills_config_entries(&self) -> Vec<one_resources::SkillConfigEntry> {
        self.skills_config
            .as_ref()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| one_resources::SkillConfigEntry {
                        path: e.path.clone(),
                        enabled: e.enabled,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_skill_enabled(&mut self, path: &std::path::Path, enabled: bool) {
        let mut entries = self.skills_config.clone().unwrap_or_default();
        let mut rs: Vec<one_resources::SkillConfigEntry> = entries
            .iter()
            .map(|e| one_resources::SkillConfigEntry {
                path: e.path.clone(),
                enabled: e.enabled,
            })
            .collect();
        one_resources::set_skill_enabled(&mut rs, path, enabled);
        entries = rs
            .into_iter()
            .map(|e| SkillConfigEntry {
                path: e.path,
                enabled: e.enabled,
            })
            .collect();
        // Drop entries that are enabled (default) to keep the file tidy —
        // only persist explicit disables (and re-enables that were previously disabled).
        // Keep both true and false so user intent is explicit like Codex.
        self.skills_config = if entries.is_empty() {
            None
        } else {
            Some(entries)
        };
    }
}

fn settings_path() -> PathBuf {
    one_session::agent_dir().join("settings.json")
}

pub fn load() -> Settings {
    let path = settings_path();
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(s) = serde_json::from_str::<Settings>(&data) {
            return s;
        }
    }
    // Migrate legacy preferences.json once.
    if let Some(prefs) = preferences::load() {
        let s = Settings {
            provider: Some(prefs.provider),
            model: Some(prefs.model),
            ..Default::default()
        };
        let _ = save(&s);
        return s;
    }
    Settings::default()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(settings)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, data)
}

pub fn path_display() -> String {
    settings_path().display().to_string()
}

/// Apply a single key/value (used by `/settings key value`).
pub fn set_key(settings: &mut Settings, key: &str, value: &str) -> Result<(), String> {
    match key.trim().to_ascii_lowercase().as_str() {
        "provider" => {
            settings.provider = Some(value.trim().to_string());
        }
        "model" => {
            settings.model = Some(value.trim().to_string());
        }
        "enabled_models" | "enabled-models" | "enabledmodels" => {
            // Empty / "all" / "*" clears the filter (show every catalog model).
            let v = value.trim();
            if v.is_empty()
                || matches!(
                    v.to_ascii_lowercase().as_str(),
                    "all" | "*" | "none" | "clear" | "default"
                )
            {
                settings.enabled_models = None;
            } else {
                let mut specs: Vec<String> = v
                    .split([',', '\n', ';'])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                specs.sort();
                specs.dedup();
                settings.enabled_models = if specs.is_empty() { None } else { Some(specs) };
            }
        }
        "thinking" => {
            let v = value.trim().to_ascii_lowercase();
            if !matches!(v.as_str(), "off" | "low" | "medium" | "high") {
                return Err("thinking must be off|low|medium|high".into());
            }
            settings.thinking = Some(v);
        }
        "auto_approve" | "auto-approve" | "yes" => {
            let v = value.trim().to_ascii_lowercase();
            settings.auto_approve = Some(matches!(v.as_str(), "1" | "true" | "yes" | "on"));
        }
        "permission_mode" | "permission-mode" | "permissionmode" | "permissions_mode" => {
            if let Some(m) = one_tools::PermissionMode::parse(value) {
                settings.permission_mode = Some(m.as_str().to_string());
            } else {
                return Err(
                    "permission_mode must be default|acceptEdits|auto|dontAsk|bypassPermissions"
                        .into(),
                );
            }
        }
        "context_window" | "context-window" | "context" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "context_window must be a number".to_string())?;
            settings.context_window = if n == 0 { None } else { Some(n) };
        }
        "sandbox" => {
            let v = value.trim().to_ascii_lowercase();
            if one_tools::SandboxMode::parse(&v).is_none() {
                return Err(
                    "sandbox must be workspace-write|full-access (aliases: workspace, full)".into(),
                );
            }
            // Normalize to canonical form.
            let mode = one_tools::SandboxMode::parse(&v).expect("checked above");
            settings.sandbox = Some(mode.as_str().to_string());
        }
        "add_dir" | "add-dir" | "additional_directories" => {
            let dirs: Vec<String> = value
                .split([',', ':'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if dirs.is_empty() {
                settings.additional_directories = None;
            } else {
                settings.additional_directories = Some(dirs);
            }
        }
        "bash_sandbox" | "bash-sandbox" => {
            let v = value.trim().to_ascii_lowercase();
            settings.bash_sandbox = Some(matches!(v.as_str(), "1" | "true" | "yes" | "on"));
        }
        "tool_output_max_lines" | "tool-output-max-lines" | "tool_output.max_lines" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "tool_output.max_lines must be a positive number".to_string())?;
            if n < 1 {
                return Err("tool_output.max_lines must be >= 1".into());
            }
            let mut t = settings.tool_output.clone().unwrap_or_default();
            t.max_lines = Some(n);
            settings.tool_output = Some(t);
            settings.apply_tool_output_limits();
        }
        "tool_output_max_bytes" | "tool-output-max-bytes" | "tool_output.max_bytes" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "tool_output.max_bytes must be a positive number".to_string())?;
            if n < 1 {
                return Err("tool_output.max_bytes must be >= 1".into());
            }
            let mut t = settings.tool_output.clone().unwrap_or_default();
            t.max_bytes = Some(n);
            settings.tool_output = Some(t);
            settings.apply_tool_output_limits();
        }
        // Compaction strategy: /settings compaction.ratio 0.8  ·  compaction.prune on
        "compaction.auto" | "compaction_auto" => {
            let v = value.trim().to_ascii_lowercase();
            let c = settings.compaction_mut();
            match v.as_str() {
                "1" | "true" | "yes" | "on" => c.auto = Some(true),
                "0" | "false" | "no" | "off" => c.auto = Some(false),
                "toggle" => c.auto = Some(!c.auto.unwrap_or(true)),
                other => {
                    return Err(format!(
                        "compaction.auto must be on|off|toggle (got `{other}`)"
                    ));
                }
            }
        }
        "compaction.ratio" | "compaction_ratio" => {
            let r: f64 =
                value.trim().trim_end_matches('%').parse().map_err(|_| {
                    "compaction.ratio must be a number (0–1 or percent)".to_string()
                })?;
            // Allow 70 or 0.70
            let r = if r > 1.0 && r <= 100.0 { r / 100.0 } else { r };
            if !(r > 0.0 && r <= 1.0) {
                return Err("compaction.ratio must be in (0, 1] (or 1–100 as percent)".into());
            }
            let c = settings.compaction_mut();
            c.ratio = Some(r);
            // Absolute threshold and ratio are alternatives — clear override.
            c.threshold = None;
        }
        "compaction.threshold" | "compaction_threshold" => {
            let v = value.trim().to_ascii_lowercase();
            let c = settings.compaction_mut();
            if matches!(v.as_str(), "0" | "auto" | "none" | "clear" | "") {
                c.threshold = None;
            } else {
                let n: usize = v.parse().map_err(|_| {
                    "compaction.threshold must be a positive token count or auto".to_string()
                })?;
                if n < 1 {
                    return Err("compaction.threshold must be >= 1 (or auto to use ratio)".into());
                }
                c.threshold = Some(n);
            }
        }
        "compaction.keep_recent" | "compaction_keep_recent" | "compaction.keep-recent" => {
            let n: usize = value.trim().parse().map_err(|_| {
                "compaction.keep_recent must be a positive user-turn count".to_string()
            })?;
            if n < 1 {
                return Err("compaction.keep_recent must be >= 1".into());
            }
            settings.compaction_mut().keep_recent = Some(n);
        }
        "compaction.prune" | "compaction_prune" => {
            let v = value.trim().to_ascii_lowercase();
            let c = settings.compaction_mut();
            match v.as_str() {
                "1" | "true" | "yes" | "on" => c.prune = Some(true),
                "0" | "false" | "no" | "off" => c.prune = Some(false),
                "toggle" => c.prune = Some(!c.prune.unwrap_or(true)),
                other => {
                    return Err(format!(
                        "compaction.prune must be on|off|toggle (got `{other}`)"
                    ));
                }
            }
        }
        "compaction.prefire_ratio"
        | "compaction.prefire-ratio"
        | "compaction_prefire_ratio"
        | "compaction.prefire" => {
            let r: f64 = value.trim().trim_end_matches('%').parse().map_err(|_| {
                "compaction.prefire_ratio must be a number (0–1 or percent)".to_string()
            })?;
            let r = if r > 1.0 && r <= 100.0 { r / 100.0 } else { r };
            if !(r > 0.0 && r < 1.0) {
                return Err(
                    "compaction.prefire_ratio must be in (0, 1) (or 1–99 as percent)".into(),
                );
            }
            settings.compaction_mut().prefire_ratio = Some(r);
        }
        "compaction.two_pass" | "compaction.two-pass" | "compaction_two_pass" => {
            let v = value.trim().to_ascii_lowercase();
            let c = settings.compaction_mut();
            match v.as_str() {
                "1" | "true" | "yes" | "on" => c.two_pass = Some(true),
                "0" | "false" | "no" | "off" => c.two_pass = Some(false),
                "toggle" => c.two_pass = Some(!c.two_pass.unwrap_or(false)),
                other => {
                    return Err(format!(
                        "compaction.two_pass must be on|off|toggle (got `{other}`)"
                    ));
                }
            }
        }
        "compaction.prefire_lead"
        | "compaction.prefire-lead"
        | "compaction.prefire_lead_ratio"
        | "compaction.lead" => {
            let r: f64 = value.trim().trim_end_matches('%').parse().map_err(|_| {
                "compaction.prefire_lead must be a number (0–1 or percent)".to_string()
            })?;
            let r = if r > 1.0 && r <= 100.0 { r / 100.0 } else { r };
            if !(r > 0.0 && r < 1.0) {
                return Err(
                    "compaction.prefire_lead must be in (0, 1) (or 1–99 as percent)".into(),
                );
            }
            settings.compaction_mut().prefire_lead_ratio = Some(r);
        }
        "compaction.prune_keep_last_n_turns"
        | "compaction.keep_last_n_turns"
        | "compaction.prune-keep-turns" => {
            let n: usize = value.trim().parse().map_err(|_| {
                "compaction.prune_keep_last_n_turns must be a positive number".to_string()
            })?;
            if n < 1 {
                return Err("compaction.prune_keep_last_n_turns must be >= 1".into());
            }
            settings.compaction_mut().prune_keep_last_n_turns = Some(n);
        }
        "compaction.prune_protect_tokens"
        | "compaction.prune-protect-tokens"
        | "compaction_prune_protect" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "compaction.prune_protect_tokens must be a number".to_string())?;
            settings.compaction_mut().prune_protect_tokens = Some(n);
        }
        "compaction.prune_max_chars"
        | "compaction.prune-max-chars"
        | "compaction_prune_max_chars" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "compaction.prune_max_chars must be a number".to_string())?;
            settings.compaction_mut().prune_max_chars = Some(n);
        }
        "empty_response_retries" | "empty-response-retries" | "empty_retries" | "empty-retries" => {
            let v = value.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "default" | "auto" | "clear" | "") {
                settings.empty_response_retries = None;
            } else {
                let n: usize = v.parse().map_err(|_| {
                    "empty_response_retries must be a non-negative integer (or default)".to_string()
                })?;
                settings.empty_response_retries = Some(n);
            }
        }
        "memory.enabled" | "memory_enabled" | "memory" => {
            let v = value.trim().to_ascii_lowercase();
            let m = settings.memory_mut();
            match v.as_str() {
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => m.enabled = Some(true),
                "0" | "false" | "no" | "off" | "disable" | "disabled" => m.enabled = Some(false),
                "toggle" => m.enabled = Some(!m.enabled.unwrap_or(true)),
                other => {
                    return Err(format!(
                        "memory.enabled must be on|off|toggle (got `{other}`)"
                    ));
                }
            }
        }
        "memory.index_max_lines"
        | "memory.index-max-lines"
        | "memory_index_max_lines"
        | "memory.max_lines" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "memory.index_max_lines must be a positive number".to_string())?;
            if n < 1 {
                return Err("memory.index_max_lines must be >= 1".into());
            }
            settings.memory_mut().index_max_lines = Some(n);
        }
        "memory.write" | "memory_write" => {
            let v = value.trim().to_ascii_lowercase();
            let m = settings.memory_mut();
            match v.as_str() {
                "1" | "true" | "yes" | "on" | "agent" | "enable" | "enabled" => {
                    m.write = Some(true)
                }
                "0" | "false" | "no" | "off" | "disable" | "disabled" => m.write = Some(false),
                "toggle" => m.write = Some(!m.write.unwrap_or(true)),
                other => {
                    return Err(format!(
                        "memory.write must be on|off|agent|toggle (got `{other}`)"
                    ));
                }
            }
        }
        "memory.max_lookups_per_turn"
        | "memory.max-lookups-per-turn"
        | "memory.max_lookups"
        | "memory_max_lookups" => {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| "memory.max_lookups_per_turn must be a positive number".to_string())?;
            if n < 1 {
                return Err("memory.max_lookups_per_turn must be >= 1".into());
            }
            settings.memory_mut().max_lookups_per_turn = Some(n);
        }
        "memory.subagent" | "memory_subagent" => {
            let v = value.trim().to_ascii_lowercase();
            match v.as_str() {
                "off" | "0" | "false" | "none" | "no" => {
                    settings.memory_mut().subagent = Some("off".into());
                }
                "index" | "on" | "true" | "1" | "l2" | "yes" => {
                    settings.memory_mut().subagent = Some("index".into());
                }
                other => {
                    return Err(format!("memory.subagent must be off|index (got `{other}`)"));
                }
            }
        }
        "memory.archive_compaction"
        | "memory.archive-compaction"
        | "memory_archive_compaction"
        | "memory.archive" => {
            let v = value.trim().to_ascii_lowercase();
            let m = settings.memory_mut();
            match v.as_str() {
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => {
                    m.archive_compaction = Some(true)
                }
                "0" | "false" | "no" | "off" | "disable" | "disabled" => {
                    m.archive_compaction = Some(false)
                }
                "toggle" => m.archive_compaction = Some(!m.archive_compaction.unwrap_or(true)),
                other => {
                    return Err(format!(
                        "memory.archive_compaction must be on|off|toggle (got `{other}`)"
                    ));
                }
            }
        }
        // Feature flags: /settings feature.subagent off  or  /settings features.subagent on
        key if key.starts_with("feature.") || key.starts_with("features.") => {
            let id = key
                .split_once('.')
                .map(|(_, rest)| rest.trim())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                return Err("feature id required (e.g. feature.subagent)".into());
            }
            // Validate against known registry when available (avoid circular import:
            // accept any non-empty id here; runtime validates known set).
            let current = settings.feature_enabled(&id, true);
            let on = match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                "toggle" => !current,
                other => {
                    return Err(format!(
                        "feature value must be on|off|toggle (got `{other}`)"
                    ));
                }
            };
            settings.set_feature(&id, on);
        }
        // Append a single rule: /settings allow Bash(cargo *)
        action @ ("allow" | "deny" | "ask") => {
            let rule = value.trim();
            if rule.is_empty() {
                return Err(format!("{action} requires a rule like Bash(git push *)"));
            }
            let rule_action = match action {
                "allow" => one_tools::RuleAction::Allow,
                "deny" => one_tools::RuleAction::Deny,
                _ => one_tools::RuleAction::Ask,
            };
            if one_tools::PermissionRule::parse(rule_action, rule).is_none() {
                return Err(format!("invalid permission rule: {rule}"));
            }
            let mut perms = settings.permissions.clone().unwrap_or_default();
            match action {
                "allow" => perms.allow.push(rule.to_string()),
                "deny" => perms.deny.push(rule.to_string()),
                _ => perms.ask.push(rule.to_string()),
            }
            settings.permissions = Some(perms);
        }
        other => {
            return Err(format!(
                "unknown setting `{other}` · known: provider model thinking auto_approve \
                 context_window sandbox add_dir bash_sandbox tool_output.max_lines \
                 tool_output.max_bytes compaction.auto|ratio|threshold|keep_recent|prune \
                 |two_pass|prefire_lead|prefire_ratio|prune_keep_last_n_turns \
                 |prune_protect_tokens|prune_max_chars \
                 memory.enabled|index_max_lines|write|max_lookups_per_turn|subagent|archive_compaction \
                 empty_response_retries \
                 feature.<id> allow deny ask"
            ));
        }
    }
    Ok(())
}

pub fn rows(settings: &Settings) -> Vec<(String, String)> {
    vec![
        (
            "provider".into(),
            settings
                .provider
                .clone()
                .unwrap_or_else(|| "(unset)".into()),
        ),
        (
            "model".into(),
            settings.model.clone().unwrap_or_else(|| "(unset)".into()),
        ),
        (
            "thinking".into(),
            settings.thinking.clone().unwrap_or_else(|| "off".into()),
        ),
        (
            "auto_approve".into(),
            settings
                .auto_approve
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or_else(|| "false".into())
                .into(),
        ),
        (
            "permission_mode".into(),
            settings
                .permission_mode
                .clone()
                .unwrap_or_else(|| "default (ask)".into()),
        ),
        (
            "context_window".into(),
            settings
                .context_window
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(auto)".into()),
        ),
        (
            "sandbox".into(),
            settings
                .sandbox
                .clone()
                .unwrap_or_else(|| "workspace-write".into()),
        ),
        (
            "add_dir".into(),
            settings
                .additional_directories
                .as_ref()
                .map(|d| d.join(", "))
                .unwrap_or_else(|| "(none)".into()),
        ),
        (
            "bash_sandbox".into(),
            settings
                .bash_sandbox
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or("true")
                .into(),
        ),
        (
            "permissions".into(),
            settings
                .permissions
                .as_ref()
                .map(|p| {
                    format!(
                        "allow={} deny={} ask={}",
                        p.allow.len(),
                        p.deny.len(),
                        p.ask.len()
                    )
                })
                .unwrap_or_else(|| "(none)".into()),
        ),
        (
            "features".into(),
            settings
                .features
                .as_ref()
                .map(|m| {
                    let mut parts: Vec<String> = m
                        .iter()
                        .map(|(k, v)| format!("{k}={}", if *v { "on" } else { "off" }))
                        .collect();
                    parts.sort();
                    if parts.is_empty() {
                        "(defaults)".into()
                    } else {
                        parts.join(" ")
                    }
                })
                .unwrap_or_else(|| "(defaults)".into()),
        ),
        {
            let lim = one_tools::tool_output_limits();
            (
                "tool_output".into(),
                format!("max_lines={} max_bytes={}", lim.max_lines, lim.max_bytes),
            )
        },
        (
            "compaction".into(),
            settings.compaction_or_default().summary_line(),
        ),
        (
            "empty_response_retries".into(),
            match settings.empty_response_retries {
                Some(n) => n.to_string(),
                None => format!(
                    "{} (default)",
                    one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES
                ),
            },
        ),
        ("path".into(), path_display()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_thinking() {
        let mut s = Settings::default();
        set_key(&mut s, "thinking", "high").unwrap();
        assert_eq!(s.thinking.as_deref(), Some("high"));
        assert!(set_key(&mut s, "thinking", "nope").is_err());
    }

    #[test]
    fn set_key_permission_mode() {
        let mut s = Settings::default();
        set_key(&mut s, "permission_mode", "always-approve").unwrap();
        assert_eq!(s.permission_mode.as_deref(), Some("bypassPermissions"));
        set_key(&mut s, "permission_mode", "auto").unwrap();
        assert_eq!(s.permission_mode.as_deref(), Some("auto"));
        set_key(&mut s, "permission_mode", "acceptEdits").unwrap();
        assert_eq!(s.permission_mode.as_deref(), Some("acceptEdits"));
        assert!(set_key(&mut s, "permission_mode", "invalid-mode").is_err());
    }

    #[test]
    fn roundtrip_json() {
        let s = Settings {
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            thinking: Some("low".into()),
            auto_approve: Some(true),
            permission_mode: Some("bypassPermissions".into()),
            context_window: Some(128_000),
            sandbox: Some("workspace-write".into()),
            additional_directories: Some(vec!["/tmp/extra".into()]),
            permissions: Some(one_tools::PermissionRules {
                allow: vec!["Bash(cargo *)".into()],
                deny: vec!["Bash(git push *)".into()],
                ask: vec![],
            }),
            bash_sandbox: Some(true),
            skills_config: Some(vec![SkillConfigEntry {
                path: "/tmp/s/SKILL.md".into(),
                enabled: false,
            }]),
            features: Some(HashMap::from([("subagent".into(), false)])),
            tool_output: Some(ToolOutputSettings {
                max_lines: Some(5000),
                max_bytes: Some(204_800),
            }),
            compaction: Some(CompactionSettings {
                auto: Some(true),
                ratio: Some(0.8),
                threshold: None,
                keep_recent: Some(10),
                prune: Some(true),
                prune_protect_tokens: Some(20_000),
                prune_max_chars: Some(1000),
                prune_keep_last_n_turns: Some(3),
                prune_soft_trim_threshold: None,
                prune_soft_trim_head: None,
                prune_soft_trim_tail: None,
                prune_hard_clear_age_turns: None,
                two_pass: Some(false),
                prefire_lead_ratio: Some(0.10),
                prefire_ratio: Some(0.85),
            }),
            memory: Some(MemorySettings {
                enabled: Some(true),
                index_max_lines: Some(40),
                write: Some(true),
                max_lookups_per_turn: Some(6),
                subagent: Some("off".into()),
                archive_compaction: Some(true),
            }),
            empty_response_retries: Some(3),
            enabled_models: Some(vec![
                "openai:gpt-4o".into(),
                "anthropic:claude-sonnet-4-20250514".into(),
            ]),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("enabledModels"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn set_key_enabled_models() {
        let mut s = Settings::default();
        set_key(&mut s, "enabled_models", "openai:gpt-4o, mock:mock-v1").unwrap();
        assert_eq!(
            s.enabled_models.as_ref().map(|v| v.as_slice()),
            Some(["mock:mock-v1".to_string(), "openai:gpt-4o".to_string()].as_slice())
        );
        set_key(&mut s, "enabled_models", "all").unwrap();
        assert!(s.enabled_models.is_none());
        set_key(&mut s, "enabledModels", "xai:grok-4.5").unwrap();
        // key is matched after to_ascii_lowercase in set_key match arms via aliases
        // — "enabledModels" lowercases to "enabledmodels"
        assert_eq!(s.enabled_models, Some(vec!["xai:grok-4.5".into()]));
    }

    #[test]
    fn empty_response_retries_set_key_and_effective() {
        let mut s = Settings::default();
        assert_eq!(
            s.empty_response_retries(),
            one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES
        );
        set_key(&mut s, "empty_response_retries", "0").unwrap();
        assert_eq!(s.empty_response_retries, Some(0));
        // Env wins over settings when set.
        std::env::set_var("ONE_EMPTY_RESPONSE_RETRIES", "5");
        assert_eq!(s.empty_response_retries(), 5);
        std::env::remove_var("ONE_EMPTY_RESPONSE_RETRIES");
        assert_eq!(s.empty_response_retries(), 0);
        set_key(&mut s, "empty_response_retries", "default").unwrap();
        assert_eq!(s.empty_response_retries, None);
        assert_eq!(
            s.empty_response_retries(),
            one_core::agent::DEFAULT_EMPTY_RESPONSE_RETRIES
        );
        assert!(set_key(&mut s, "empty_response_retries", "nope").is_err());
    }

    #[test]
    fn compaction_set_key_and_config() {
        let mut s = Settings::default();
        set_key(&mut s, "compaction.ratio", "80").unwrap();
        set_key(&mut s, "compaction.prune", "on").unwrap();
        set_key(&mut s, "compaction.keep_recent", "8").unwrap();
        let cfg = s.compaction_config(100_000);
        assert!(cfg.enabled);
        assert_eq!(cfg.token_threshold, 80_000);
        assert!(cfg.prune);
        assert_eq!(cfg.keep_recent_messages, 8);
        set_key(&mut s, "compaction.threshold", "50000").unwrap();
        let cfg2 = s.compaction_config(100_000);
        assert_eq!(cfg2.token_threshold, 50_000);
        set_key(&mut s, "compaction.auto", "off").unwrap();
        assert!(!s.compaction_config(100_000).enabled);
        assert!(s.compaction_or_default().summary_line().contains("prune"));
        set_key(&mut s, "compaction.two_pass", "on").unwrap();
        set_key(&mut s, "compaction.prefire_lead", "10").unwrap();
        set_key(&mut s, "compaction.prune_keep_last_n_turns", "4").unwrap();
        let cfg3 = s.compaction_config(100_000);
        assert!(cfg3.two_pass);
        assert!((cfg3.prefire_lead_ratio - 0.10).abs() < f64::EPSILON);
        assert_eq!(cfg3.prune_keep_last_n_turns, 4);
    }

    #[test]
    fn tool_output_set_key() {
        let mut s = Settings::default();
        set_key(&mut s, "tool_output.max_lines", "100").unwrap();
        assert_eq!(s.tool_output.as_ref().unwrap().max_lines, Some(100));
        set_key(&mut s, "tool_output.max_bytes", "4096").unwrap();
        assert_eq!(s.tool_output.as_ref().unwrap().max_bytes, Some(4096));
        assert_eq!(one_tools::tool_output_limits().max_lines, 100);
        assert_eq!(one_tools::tool_output_limits().max_bytes, 4096);
        // Restore defaults for other tests in the same process.
        one_tools::set_tool_output_limits(one_tools::ToolOutputLimits::default());
    }

    #[test]
    fn set_key_feature() {
        let mut s = Settings::default();
        set_key(&mut s, "feature.subagent", "off").unwrap();
        assert_eq!(s.feature_enabled("subagent", true), false);
        set_key(&mut s, "features.subagent", "toggle").unwrap();
        assert_eq!(s.feature_enabled("subagent", true), true);
    }

    #[test]
    fn skill_toggle_persists_path() {
        let mut s = Settings::default();
        s.set_skill_enabled(std::path::Path::new("/tmp/x/SKILL.md"), false);
        assert_eq!(s.skills_config.as_ref().unwrap().len(), 1);
        assert!(!s.skills_config.as_ref().unwrap()[0].enabled);
        s.set_skill_enabled(std::path::Path::new("/tmp/x/SKILL.md"), true);
        assert!(s.skills_config.as_ref().unwrap()[0].enabled);
    }
}
