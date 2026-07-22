//! Built-in OpenCode Zen / Go model catalogs + `models.json` seeding.
//!
//! Wire shapes follow models.dev / OpenCode docs:
//! - GPT family → OpenAI Responses (`…/responses`)
//! - Claude / MiniMax(Qwen anthropic package) → Anthropic Messages
//! - Everything else → OpenAI Chat Completions
//!
//! Auth is API-key based (subscription console); see [`crate::auth::login_opencode`].

use std::path::Path;

use crate::auth::{opencode_base_url, PROVIDER_OPENCODE, PROVIDER_OPENCODE_GO};
use crate::models_file::{load_models_file, save_models_file, ModelsConfig};
use crate::openai::ProviderApi;
use crate::registry::{ModelEntry, ProviderConfig};

/// Wire family for a Zen/Go model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpencodeWire {
    Completions,
    Responses,
    AnthropicMessages,
}

/// One curated OpenCode model definition.
#[derive(Debug, Clone, Copy)]
pub struct OpencodeModelDef {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u32,
    pub wire: OpencodeWire,
    pub reasoning: bool,
}

/// OpenCode Go subscription catalog (curated; stable open models).
pub const OPENCODE_GO_BUILTIN_MODELS: &[OpencodeModelDef] = &[
    OpencodeModelDef {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        context_window: 1_000_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        context_window: 1_000_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "kimi-k2.6",
        name: "Kimi K2.6",
        context_window: 262_144,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "kimi-k2.7-code",
        name: "Kimi K2.7 Code",
        context_window: 262_144,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "kimi-k3",
        name: "Kimi K3",
        context_window: 262_144,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "grok-4.5",
        name: "Grok 4.5",
        context_window: 500_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "glm-5.2",
        name: "GLM 5.2",
        context_window: 200_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "glm-5.1",
        name: "GLM 5.1",
        context_window: 200_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "mimo-v2.5",
        name: "MiMo V2.5",
        context_window: 262_144,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "mimo-v2.5-pro",
        name: "MiMo V2.5 Pro",
        context_window: 262_144,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "minimax-m2.7",
        name: "MiniMax M2.7",
        context_window: 200_000,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "minimax-m3",
        name: "MiniMax M3",
        context_window: 200_000,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "qwen3.7-plus",
        name: "Qwen3.7 Plus",
        context_window: 262_144,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "qwen3.7-max",
        name: "Qwen3.7 Max",
        context_window: 262_144,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
];

/// OpenCode Zen gateway catalog (subset of popular coding models).
pub const OPENCODE_ZEN_BUILTIN_MODELS: &[OpencodeModelDef] = &[
    OpencodeModelDef {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        context_window: 1_000_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "kimi-k2.6",
        name: "Kimi K2.6",
        context_window: 262_144,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "grok-4.5",
        name: "Grok 4.5",
        context_window: 500_000,
        wire: OpencodeWire::Completions,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "big-pickle",
        name: "Big Pickle (free)",
        context_window: 200_000,
        wire: OpencodeWire::Completions,
        reasoning: false,
    },
    OpencodeModelDef {
        id: "claude-sonnet-4-5",
        name: "Claude Sonnet 4.5",
        context_window: 1_000_000,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "claude-opus-4-6",
        name: "Claude Opus 4.6",
        context_window: 1_000_000,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "claude-haiku-4-5",
        name: "Claude Haiku 4.5",
        context_window: 200_000,
        wire: OpencodeWire::AnthropicMessages,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "gpt-5.4",
        name: "GPT 5.4",
        context_window: 272_000,
        wire: OpencodeWire::Responses,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "gpt-5.5",
        name: "GPT 5.5",
        context_window: 272_000,
        wire: OpencodeWire::Responses,
        reasoning: true,
    },
    OpencodeModelDef {
        id: "gpt-5.3-codex",
        name: "GPT 5.3 Codex",
        context_window: 272_000,
        wire: OpencodeWire::Responses,
        reasoning: true,
    },
];

pub const OPENCODE_GO_DEFAULT_MODEL: &str = "deepseek-v4-flash";
pub const OPENCODE_ZEN_DEFAULT_MODEL: &str = "kimi-k2.6";

fn wire_api_str(w: OpencodeWire) -> &'static str {
    match w {
        OpencodeWire::Completions => "openai-completions",
        OpencodeWire::Responses => "openai-responses",
        OpencodeWire::AnthropicMessages => "anthropic-messages",
    }
}

fn model_entries(provider: &str, defs: &[OpencodeModelDef]) -> Vec<ModelEntry> {
    let base = opencode_base_url(provider).to_string();
    defs.iter()
        .map(|m| ModelEntry {
            provider: provider.into(),
            id: m.id.into(),
            name: m.name.into(),
            context_window: Some(m.context_window),
            api: Some(wire_api_str(m.wire).into()),
            base_url: Some(base.clone()),
            api_key: None,
            reasoning: Some(m.reasoning),
            thinking_level_map: None,
            compat: None,
        })
        .collect()
}

/// Result of seeding OpenCode models into `models.json`.
#[derive(Debug, Clone)]
pub struct OpencodeSeedReport {
    pub path: std::path::PathBuf,
    pub provider: String,
    pub added: usize,
    pub updated: usize,
    pub total: usize,
    pub default_model: String,
}

/// Seed Zen and/or Go catalogs. `provider` is `opencode`, `opencode-go`, or `both`.
pub fn seed_opencode_models(path: &Path, provider: &str) -> Result<OpencodeSeedReport, String> {
    let which = match provider {
        "both" | "all" => "both",
        PROVIDER_OPENCODE | "zen" => PROVIDER_OPENCODE,
        PROVIDER_OPENCODE_GO | "go" => PROVIDER_OPENCODE_GO,
        other => {
            return Err(format!(
                "seed provider `{other}` · use opencode | opencode-go | both"
            ))
        }
    };

    let mut cfg = if path.exists() {
        crate::models_file::try_load_models_file(path)?
    } else {
        ModelsConfig::with_defaults()
    };

    let mut added = 0usize;
    let mut updated = 0usize;
    let mut default_model = String::new();
    let mut primary = PROVIDER_OPENCODE_GO;

    let targets: &[(&str, &[OpencodeModelDef], &str)] = match which {
        "both" => &[
            (
                PROVIDER_OPENCODE_GO,
                OPENCODE_GO_BUILTIN_MODELS,
                OPENCODE_GO_DEFAULT_MODEL,
            ),
            (
                PROVIDER_OPENCODE,
                OPENCODE_ZEN_BUILTIN_MODELS,
                OPENCODE_ZEN_DEFAULT_MODEL,
            ),
        ],
        PROVIDER_OPENCODE => &[(
            PROVIDER_OPENCODE,
            OPENCODE_ZEN_BUILTIN_MODELS,
            OPENCODE_ZEN_DEFAULT_MODEL,
        )],
        _ => &[(
            PROVIDER_OPENCODE_GO,
            OPENCODE_GO_BUILTIN_MODELS,
            OPENCODE_GO_DEFAULT_MODEL,
        )],
    };

    for (pid, defs, def_model) in targets {
        primary = *pid;
        default_model = def_model.to_string();
        for entry in model_entries(pid, defs) {
            let existed = cfg.find_model(pid, &entry.id).is_some();
            cfg.upsert_model(entry);
            if existed {
                updated += 1;
            } else {
                added += 1;
            }
        }
        ensure_provider_block(&mut cfg, pid, def_model);
    }

    save_models_file(path, &cfg)?;

    let total = if which == "both" {
        cfg.registry.list_by_provider(PROVIDER_OPENCODE).len()
            + cfg.registry.list_by_provider(PROVIDER_OPENCODE_GO).len()
    } else {
        cfg.registry.list_by_provider(primary).len()
    };

    Ok(OpencodeSeedReport {
        path: path.to_path_buf(),
        provider: if which == "both" {
            "opencode+opencode-go".into()
        } else {
            primary.into()
        },
        added,
        updated,
        total,
        default_model,
    })
}

fn ensure_provider_block(cfg: &mut ModelsConfig, provider: &str, default_model: &str) {
    let base = opencode_base_url(provider).to_string();
    // Default wire: completions (widest Go surface). Per-model api overrides still apply.
    let api = ProviderApi::OpenaiCompletions;
    cfg.ensure_provider(provider);
    if let Some(p) = cfg.provider_mut(provider) {
        p.base_url = Some(base.clone());
        p.api = Some(api);
        p.provider_type = Some("openai-completions".into());
        p.default_model = Some(default_model.into());
    } else {
        cfg.upsert_provider(ProviderConfig {
            id: provider.into(),
            provider_type: Some("openai-completions".into()),
            base_url: Some(base),
            api: Some(api),
            api_key: None,
            api_key_raw: None,
            default_model: Some(default_model.into()),
            compat: None,
        });
    }
}

/// Seed into default `~/.one/agent/models.json`.
pub fn seed_opencode_models_default(provider: &str) -> Result<OpencodeSeedReport, String> {
    seed_opencode_models(&default_models_json_path(), provider)
}

fn default_models_json_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".one/agent/models.json")
}

/// Infer wire from model id when models.json has no `api` (fallback).
pub fn infer_opencode_wire(model_id: &str) -> OpencodeWire {
    let m = model_id.to_ascii_lowercase();
    if m.starts_with("gpt-") {
        OpencodeWire::Responses
    } else if m.starts_with("claude-")
        || m.starts_with("minimax-")
        || m.starts_with("qwen3")
        || m.starts_with("qwen-")
    {
        OpencodeWire::AnthropicMessages
    } else {
        OpencodeWire::Completions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn go_catalog_has_defaults() {
        let ids: Vec<_> = OPENCODE_GO_BUILTIN_MODELS.iter().map(|m| m.id).collect();
        assert!(ids.contains(&"deepseek-v4-flash"));
        assert!(ids.contains(&"kimi-k2.6"));
        assert!(ids.contains(&"minimax-m2.7"));
    }

    #[test]
    fn seed_go_writes_models() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("one-opencode-seed-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        std::fs::write(
            &path,
            "{\n  \"includeDefaults\": false,\n  \"providers\": {}\n}\n",
        )
        .unwrap();

        let report = seed_opencode_models(&path, PROVIDER_OPENCODE_GO).unwrap();
        assert_eq!(report.added, OPENCODE_GO_BUILTIN_MODELS.len());
        assert_eq!(report.default_model, OPENCODE_GO_DEFAULT_MODEL);

        let cfg = load_models_file(&path);
        assert!(cfg
            .find_model(PROVIDER_OPENCODE_GO, "deepseek-v4-flash")
            .is_some());
        assert_eq!(
            cfg.find_model(PROVIDER_OPENCODE_GO, "minimax-m2.7")
                .and_then(|m| m.api.clone())
                .as_deref(),
            Some("anthropic-messages")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn infer_wire() {
        assert_eq!(infer_opencode_wire("gpt-5.4"), OpencodeWire::Responses);
        assert_eq!(
            infer_opencode_wire("claude-sonnet-4-5"),
            OpencodeWire::AnthropicMessages
        );
        assert_eq!(
            infer_opencode_wire("deepseek-v4-flash"),
            OpencodeWire::Completions
        );
    }
}
