//! Pi-compatible provider/model `compat` flags for OpenAI-compatible APIs.
//!
//! Mirrors `@earendil-works/pi-ai` `OpenAICompletionsCompat` (and a thin Anthropic
//! subset). Provider-level and model-level entries merge with auto-detection from
//! provider id / base URL when fields are omitted.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use one_core::agent::ThinkingLevel;

/// How to encode thinking / reasoning on chat/completions request bodies.
///
/// Aligns with Pi `OpenAICompletionsCompat.thinkingFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingFormat {
    /// Top-level `reasoning_effort` (OpenAI).
    #[default]
    #[serde(alias = "openai")]
    Openai,
    /// `reasoning: { effort }` (+ `include_reasoning` when useful).
    Openrouter,
    /// `thinking: { type }` + optional `reasoning_effort`.
    Deepseek,
    /// `reasoning: { enabled }` + optional `reasoning_effort`.
    Together,
    /// `thinking: { type: enabled|disabled }` (+ optional effort).
    Zai,
    /// Top-level `enable_thinking: bool`.
    Qwen,
    /// `chat_template_kwargs` from [`OpenAiCompletionsCompat::chat_template_kwargs`].
    #[serde(rename = "chat-template")]
    ChatTemplate,
    /// `chat_template_kwargs.enable_thinking` + `preserve_thinking`.
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    /// Top-level `thinking: string` (mapped effort).
    #[serde(rename = "string-thinking")]
    StringThinking,
    /// `reasoning: { effort }` only when mapped effort is non-null.
    #[serde(rename = "ant-ling")]
    AntLing,
}

impl ThinkingFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "reasoning_effort" | "reasoning-effort" => Some(Self::Openai),
            "openrouter" | "or" => Some(Self::Openrouter),
            "deepseek" => Some(Self::Deepseek),
            "together" => Some(Self::Together),
            "zai" | "z.ai" | "z-ai" => Some(Self::Zai),
            "qwen" => Some(Self::Qwen),
            "chat-template" | "chat_template" => Some(Self::ChatTemplate),
            "qwen-chat-template" | "qwen_chat_template" => Some(Self::QwenChatTemplate),
            "string-thinking" | "string_thinking" => Some(Self::StringThinking),
            "ant-ling" | "antling" | "ant_ling" => Some(Self::AntLing),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Openrouter => "openrouter",
            Self::Deepseek => "deepseek",
            Self::Together => "together",
            Self::Zai => "zai",
            Self::Qwen => "qwen",
            Self::ChatTemplate => "chat-template",
            Self::QwenChatTemplate => "qwen-chat-template",
            Self::StringThinking => "string-thinking",
            Self::AntLing => "ant-ling",
        }
    }
}

/// `max_tokens` vs `max_completion_tokens` for OpenAI-compatible chat APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    #[default]
    #[serde(rename = "max_completion_tokens")]
    MaxCompletionTokens,
    #[serde(rename = "max_tokens")]
    MaxTokens,
}

impl MaxTokensField {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "max_completion_tokens" | "max-completion-tokens" => Some(Self::MaxCompletionTokens),
            "max_tokens" | "max-tokens" => Some(Self::MaxTokens),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxTokens => "max_tokens",
        }
    }
}

/// Pi `thinkingLevelMap`: maps agent levels (`off`/`low`/…) to provider strings.
///
/// - missing key → use default effort label for that level
/// - `"high": "max"` → send `"max"` when level is high
/// - `"xhigh": null` → level unsupported (skip / clamp away)
pub type ThinkingLevelMap = std::collections::BTreeMap<String, Option<String>>;

/// Resolve a thinking level through an optional map.
///
/// Returns:
/// - `MapResult::Default` — use standard effort string for the level
/// - `MapResult::Mapped(s)` — send this string
/// - `MapResult::Unsupported` — level disabled by map (`null`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapResult {
    Default,
    Mapped(String),
    Unsupported,
}

/// Lookup `level` in a thinking level map (Pi semantics).
pub fn resolve_thinking_level_map(map: &ThinkingLevelMap, level: ThinkingLevel) -> MapResult {
    let key = match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    };
    // Also accept aliases stored under minimal/xhigh/max for future levels.
    if let Some(v) = map.get(key) {
        return match v {
            Some(s) => MapResult::Mapped(s.clone()),
            None => MapResult::Unsupported,
        };
    }
    MapResult::Default
}

/// Parse `"off=null,high=high,max=max"` or JSON object into a map.
pub fn parse_thinking_level_map(raw: &str) -> Result<ThinkingLevelMap, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(ThinkingLevelMap::new());
    }
    if s.starts_with('{') {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("thinkingLevelMap JSON: {e}"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| "thinkingLevelMap must be a JSON object".to_string())?;
        let mut map = ThinkingLevelMap::new();
        for (k, val) in obj {
            if val.is_null() {
                map.insert(k.clone(), None);
            } else if let Some(s) = val.as_str() {
                map.insert(k.clone(), Some(s.to_string()));
            } else {
                return Err(format!(
                    "thinkingLevelMap.{k}: expected string or null, got {val}"
                ));
            }
        }
        return Ok(map);
    }
    // Compact form: key=value,key=null
    let mut map = ThinkingLevelMap::new();
    for part in s.split(|c| c == ',' || c == ';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part
            .split_once('=')
            .or_else(|| part.split_once(':'))
            .ok_or_else(|| format!("thinkingLevelMap entry `{part}` needs key=value"))?;
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if v.eq_ignore_ascii_case("null") || v == "-" || v.is_empty() {
            map.insert(k, None);
        } else {
            map.insert(k, Some(v.to_string()));
        }
    }
    Ok(map)
}

/// Format map for display / inline edit.
pub fn format_thinking_level_map(map: &ThinkingLevelMap) -> String {
    if map.is_empty() {
        return String::new();
    }
    map.iter()
        .map(|(k, v)| match v {
            Some(s) => format!("{k}={s}"),
            None => format!("{k}=null"),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Session-affinity header shape (Pi `sessionAffinityFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAffinityFormat {
    #[default]
    Openai,
    #[serde(rename = "openai-nosession")]
    OpenaiNosession,
    Openrouter,
}

/// Partial / on-disk compat overrides (all fields optional).
///
/// Provider-level and model-level blocks use this shape. Missing fields fall
/// through to auto-detection ([`OpenAiCompletionsCompat::detect`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenAiCompletionsCompat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    /// Free-form `chat_template_kwargs` template (Pi `$var` resolved at request time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_router_routing: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vercel_gateway_routing: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    /// Only `"anthropic"` is recognized today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
}

impl OpenAiCompletionsCompat {
    /// Whether any field is set (for skip-serializing empty objects).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Merge `override` onto `self` (override wins when `Some`).
    pub fn merge_override(&self, over: &Self) -> Self {
        Self {
            supports_store: over.supports_store.or(self.supports_store),
            supports_developer_role: over
                .supports_developer_role
                .or(self.supports_developer_role),
            supports_reasoning_effort: over
                .supports_reasoning_effort
                .or(self.supports_reasoning_effort),
            supports_usage_in_streaming: over
                .supports_usage_in_streaming
                .or(self.supports_usage_in_streaming),
            max_tokens_field: over.max_tokens_field.or(self.max_tokens_field),
            requires_tool_result_name: over
                .requires_tool_result_name
                .or(self.requires_tool_result_name),
            requires_assistant_after_tool_result: over
                .requires_assistant_after_tool_result
                .or(self.requires_assistant_after_tool_result),
            requires_thinking_as_text: over
                .requires_thinking_as_text
                .or(self.requires_thinking_as_text),
            requires_reasoning_content_on_assistant_messages: over
                .requires_reasoning_content_on_assistant_messages
                .or(self.requires_reasoning_content_on_assistant_messages),
            thinking_format: over.thinking_format.or(self.thinking_format),
            chat_template_kwargs: over
                .chat_template_kwargs
                .clone()
                .or_else(|| self.chat_template_kwargs.clone()),
            open_router_routing: over
                .open_router_routing
                .clone()
                .or_else(|| self.open_router_routing.clone()),
            vercel_gateway_routing: over
                .vercel_gateway_routing
                .clone()
                .or_else(|| self.vercel_gateway_routing.clone()),
            zai_tool_stream: over.zai_tool_stream.or(self.zai_tool_stream),
            supports_strict_mode: over.supports_strict_mode.or(self.supports_strict_mode),
            cache_control_format: over
                .cache_control_format
                .clone()
                .or_else(|| self.cache_control_format.clone()),
            send_session_affinity_headers: over
                .send_session_affinity_headers
                .or(self.send_session_affinity_headers),
            session_affinity_format: over
                .session_affinity_format
                .or(self.session_affinity_format),
            supports_long_cache_retention: over
                .supports_long_cache_retention
                .or(self.supports_long_cache_retention),
        }
    }

    /// Auto-detect defaults from provider id + base URL + model id (Pi `detectCompat`).
    pub fn detect(provider: &str, base_url: &str, model_id: &str) -> Self {
        let p = provider.to_ascii_lowercase();
        let url = base_url.to_ascii_lowercase();
        let mid = model_id.to_ascii_lowercase();

        let is_zai = p == "zai"
            || p == "zai-coding-cn"
            || p == "zhipu"
            || url.contains("api.z.ai")
            || url.contains("open.bigmodel.cn");
        let is_together =
            p == "together" || url.contains("api.together.ai") || url.contains("api.together.xyz");
        let is_moonshot = p == "moonshotai"
            || p == "moonshotai-cn"
            || p == "moonshot"
            || p == "kimi"
            || p == "kimi-coding"
            || url.contains("api.moonshot.")
            || url.contains("api.kimi.com");
        let is_openrouter = p == "openrouter" || url.contains("openrouter.ai");
        let is_cloudflare_workers =
            p == "cloudflare-workers-ai" || url.contains("api.cloudflare.com/client/v4/accounts");
        let is_cloudflare_gateway =
            p == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
        let is_nvidia = p == "nvidia" || p == "nim" || url.contains("integrate.api.nvidia.com");
        let is_ant_ling = p == "ant-ling" || url.contains("api.ant-ling.com");
        let is_deepseek = p == "deepseek" || url.contains("deepseek.com");
        let is_grok = p == "xai" || p == "grok" || url.contains("api.x.ai");
        let is_ollama = p == "ollama"
            || url.contains("11434")
            || url.contains("localhost:11434")
            || url.contains("127.0.0.1:11434");
        let is_groq = p == "groq" || url.contains("api.groq.com");
        let is_fireworks = p == "fireworks" || url.contains("api.fireworks.ai");
        let is_cerebras = p == "cerebras" || url.contains("cerebras.ai");
        let is_mistral = p == "mistral" || url.contains("api.mistral.ai");
        let is_huggingface =
            p == "huggingface" || p == "hf" || url.contains("api-inference.huggingface")
                || url.contains("router.huggingface.co");
        let is_lmstudio = p == "lmstudio"
            || p == "lm-studio"
            || url.contains("1234")
            || url.contains("localhost:1234");
        let is_vllm = p == "vllm" || url.contains("/v1") && (p == "vllm" || url.contains("8000"));
        let is_sglang = p == "sglang";
        let is_siliconflow = p == "siliconflow" || url.contains("siliconflow.cn");
        let is_minimax = p == "minimax" || p == "minimax-cn" || url.contains("api.minimax");
        let is_opencode = p == "opencode" || p == "opencode-go" || url.contains("opencode.ai");
        let is_chutes = url.contains("chutes.ai") || p == "chutes";
        let is_vercel_gateway = p == "vercel-ai-gateway" || url.contains("ai-gateway.vercel.sh");
        let is_github_copilot = p == "github-copilot" || p == "copilot";
        let is_localish = is_ollama || is_lmstudio || is_vllm || is_sglang;

        let is_non_standard = is_nvidia
            || is_cerebras
            || is_grok
            || is_together
            || is_chutes
            || is_deepseek
            || is_zai
            || is_moonshot
            || is_opencode
            || is_cloudflare_workers
            || is_cloudflare_gateway
            || is_ant_ling
            || is_localish
            || is_groq
            || is_fireworks
            || is_mistral
            || is_huggingface
            || is_siliconflow
            || is_minimax
            || is_github_copilot;

        let use_max_tokens = is_chutes
            || is_moonshot
            || is_cloudflare_gateway
            || is_together
            || is_nvidia
            || is_ant_ling
            || is_localish
            || is_groq
            || is_fireworks
            || is_mistral
            || is_huggingface
            || is_siliconflow
            || is_minimax;

        let is_openrouter_developer =
            is_openrouter && (mid.starts_with("anthropic/") || mid.starts_with("openai/"));

        let cache_control = if (is_openrouter && mid.starts_with("anthropic/"))
            || (is_vercel_gateway && mid.contains("anthropic"))
        {
            Some("anthropic".to_string())
        } else {
            None
        };

        let thinking_format = if is_deepseek {
            ThinkingFormat::Deepseek
        } else if is_zai {
            ThinkingFormat::Zai
        } else if is_together {
            ThinkingFormat::Together
        } else if is_ant_ling {
            ThinkingFormat::AntLing
        } else if is_openrouter || is_vercel_gateway {
            ThinkingFormat::Openrouter
        } else if mid.contains("qwen") && is_localish {
            ThinkingFormat::QwenChatTemplate
        } else {
            ThinkingFormat::Openai
        };

        let supports_reasoning_effort = !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_gateway
            && !is_nvidia
            && !is_ant_ling
            && !is_localish
            && !is_groq
            && !is_mistral
            && !is_minimax;

        // Local / many OpenAI-compat proxies don't accept `developer` role.
        let supports_developer_role = is_openrouter_developer
            || (!is_non_standard && !is_openrouter && !is_vercel_gateway);

        Self {
            supports_store: Some(!is_non_standard),
            supports_developer_role: Some(supports_developer_role),
            supports_reasoning_effort: Some(supports_reasoning_effort),
            supports_usage_in_streaming: Some(!is_localish),
            max_tokens_field: Some(if use_max_tokens {
                MaxTokensField::MaxTokens
            } else {
                MaxTokensField::MaxCompletionTokens
            }),
            requires_tool_result_name: Some(is_mistral || is_groq),
            requires_assistant_after_tool_result: Some(false),
            requires_thinking_as_text: Some(false),
            requires_reasoning_content_on_assistant_messages: Some(is_deepseek),
            thinking_format: Some(thinking_format),
            chat_template_kwargs: None,
            open_router_routing: None,
            vercel_gateway_routing: None,
            zai_tool_stream: Some(is_zai),
            supports_strict_mode: Some(
                !is_moonshot
                    && !is_together
                    && !is_cloudflare_gateway
                    && !is_nvidia
                    && !is_localish
                    && !is_mistral
                    && !is_groq,
            ),
            cache_control_format: cache_control,
            send_session_affinity_headers: Some(is_fireworks),
            session_affinity_format: if is_openrouter {
                Some(SessionAffinityFormat::Openrouter)
            } else {
                None
            },
            supports_long_cache_retention: Some(
                !(is_together
                    || is_cloudflare_workers
                    || is_cloudflare_gateway
                    || is_nvidia
                    || is_ant_ling
                    || is_localish
                    || is_groq),
            ),
        }
    }

    /// Resolve against auto-detection (explicit fields win).
    pub fn resolve(&self, provider: &str, base_url: &str, model_id: &str) -> ResolvedOpenAiCompat {
        let detected = Self::detect(provider, base_url, model_id);
        let merged = detected.merge_override(self);
        ResolvedOpenAiCompat {
            supports_store: merged.supports_store.unwrap_or(false),
            supports_developer_role: merged.supports_developer_role.unwrap_or(false),
            supports_reasoning_effort: merged.supports_reasoning_effort.unwrap_or(true),
            supports_usage_in_streaming: merged.supports_usage_in_streaming.unwrap_or(true),
            max_tokens_field: merged
                .max_tokens_field
                .unwrap_or(MaxTokensField::MaxCompletionTokens),
            requires_tool_result_name: merged.requires_tool_result_name.unwrap_or(false),
            requires_assistant_after_tool_result: merged
                .requires_assistant_after_tool_result
                .unwrap_or(false),
            requires_thinking_as_text: merged.requires_thinking_as_text.unwrap_or(false),
            requires_reasoning_content_on_assistant_messages: merged
                .requires_reasoning_content_on_assistant_messages
                .unwrap_or(false),
            thinking_format: merged.thinking_format.unwrap_or(ThinkingFormat::Openai),
            chat_template_kwargs: merged.chat_template_kwargs,
            open_router_routing: merged.open_router_routing,
            vercel_gateway_routing: merged.vercel_gateway_routing,
            zai_tool_stream: merged.zai_tool_stream.unwrap_or(false),
            supports_strict_mode: merged.supports_strict_mode.unwrap_or(true),
            cache_control_format: merged.cache_control_format,
            send_session_affinity_headers: merged.send_session_affinity_headers.unwrap_or(false),
            session_affinity_format: merged.session_affinity_format,
            supports_long_cache_retention: merged.supports_long_cache_retention.unwrap_or(true),
            thinking_level_map: ThinkingLevelMap::new(),
        }
    }
}

/// Fully resolved compat (no `Option` — ready for request building).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOpenAiCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub max_tokens_field: MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: ThinkingFormat,
    pub chat_template_kwargs: Option<Value>,
    pub open_router_routing: Option<Value>,
    pub vercel_gateway_routing: Option<Value>,
    pub zai_tool_stream: bool,
    pub supports_strict_mode: bool,
    pub cache_control_format: Option<String>,
    pub send_session_affinity_headers: bool,
    pub session_affinity_format: Option<SessionAffinityFormat>,
    pub supports_long_cache_retention: bool,
    /// Per-model thinking level remaps (Pi `thinkingLevelMap`).
    pub thinking_level_map: ThinkingLevelMap,
}

impl ResolvedOpenAiCompat {
    /// Attach a thinking level map (builder-style).
    pub fn with_thinking_level_map(mut self, map: ThinkingLevelMap) -> Self {
        self.thinking_level_map = map;
        self
    }
}

impl Default for ResolvedOpenAiCompat {
    fn default() -> Self {
        // Neutral OpenAI-compatible defaults (official OpenAI-ish).
        OpenAiCompletionsCompat::detect("openai", "https://api.openai.com/v1", "gpt-4o").resolve(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o",
        )
    }
}

impl ResolvedOpenAiCompat {
    /// System / developer role for the system prompt message.
    pub fn system_role(&self, reasoning_model: bool) -> &'static str {
        if reasoning_model && self.supports_developer_role {
            "developer"
        } else {
            "system"
        }
    }

    /// Map agent thinking level → optional provider effort string (Pi `thinkingLevelMap`).
    ///
    /// Returns `None` when the level is unsupported (`null` in the map) or off
    /// with no explicit off mapping.
    pub fn mapped_effort(&self, level: ThinkingLevel) -> Option<String> {
        match resolve_thinking_level_map(&self.thinking_level_map, level) {
            MapResult::Unsupported => None,
            MapResult::Mapped(s) => Some(s),
            MapResult::Default => level.effort().map(|s| s.to_string()).or_else(|| {
                // Explicit off mapping may use thinkingLevelMap.off = "none".
                if matches!(level, ThinkingLevel::Off) {
                    None
                } else {
                    None
                }
            }),
        }
    }

    /// Whether `level` is allowed by the thinking level map.
    pub fn level_supported(&self, level: ThinkingLevel) -> bool {
        !matches!(
            resolve_thinking_level_map(&self.thinking_level_map, level),
            MapResult::Unsupported
        )
    }

    /// Apply thinking / reasoning fields for a chat/completions body (Pi `buildParams`).
    pub fn apply_thinking(&self, body: &mut Value, level: ThinkingLevel, reasoning_model: bool) {
        if !reasoning_model && !level.is_enabled() {
            return;
        }
        // Gate: Pi only emits thinking params when `model.reasoning` is true.
        // We still honor an explicit non-Off thinking level for models that omit
        // the flag (common in hand-written models.json).
        let active = reasoning_model || level.is_enabled();
        if !active {
            return;
        }

        if !self.level_supported(level) {
            // Level disabled via thinkingLevelMap (null entry).
            return;
        }

        let effort = self.mapped_effort(level);
        let effort_ref = effort.as_deref();
        let obj = match body.as_object_mut() {
            Some(o) => o,
            None => return,
        };

        match self.thinking_format {
            ThinkingFormat::Zai => {
                obj.insert(
                    "thinking".into(),
                    json!({ "type": if effort_ref.is_some() { "enabled" } else { "disabled" } }),
                );
                if let Some(e) = effort_ref {
                    if self.supports_reasoning_effort {
                        obj.insert("reasoning_effort".into(), json!(e));
                    }
                }
            }
            ThinkingFormat::Qwen => {
                obj.insert("enable_thinking".into(), json!(effort_ref.is_some()));
            }
            ThinkingFormat::QwenChatTemplate => {
                obj.insert(
                    "chat_template_kwargs".into(),
                    json!({
                        "enable_thinking": effort_ref.is_some(),
                        "preserve_thinking": true,
                    }),
                );
            }
            ThinkingFormat::ChatTemplate => {
                if let Some(kwargs) =
                    build_chat_template_kwargs(&self.chat_template_kwargs, effort_ref)
                {
                    obj.insert("chat_template_kwargs".into(), kwargs);
                }
            }
            ThinkingFormat::Deepseek => {
                if effort_ref.is_some() {
                    obj.insert("thinking".into(), json!({ "type": "enabled" }));
                } else {
                    obj.insert("thinking".into(), json!({ "type": "disabled" }));
                }
                if let Some(e) = effort_ref {
                    if self.supports_reasoning_effort {
                        obj.insert("reasoning_effort".into(), json!(e));
                    }
                }
            }
            ThinkingFormat::Openrouter => {
                if let Some(e) = effort_ref {
                    obj.insert("reasoning".into(), json!({ "effort": e }));
                    obj.insert("include_reasoning".into(), json!(true));
                } else {
                    // Prefer explicit off mapping; default "none".
                    let off = match resolve_thinking_level_map(
                        &self.thinking_level_map,
                        ThinkingLevel::Off,
                    ) {
                        MapResult::Mapped(s) => s,
                        _ => "none".into(),
                    };
                    obj.insert("reasoning".into(), json!({ "effort": off }));
                }
            }
            ThinkingFormat::AntLing => {
                if let Some(e) = effort_ref {
                    obj.insert("reasoning".into(), json!({ "effort": e }));
                }
            }
            ThinkingFormat::Together => {
                obj.insert(
                    "reasoning".into(),
                    json!({ "enabled": effort_ref.is_some() }),
                );
                if let Some(e) = effort_ref {
                    if self.supports_reasoning_effort {
                        obj.insert("reasoning_effort".into(), json!(e));
                    }
                }
            }
            ThinkingFormat::StringThinking => {
                if let Some(e) = effort_ref {
                    obj.insert("thinking".into(), json!(e));
                } else {
                    let off = match resolve_thinking_level_map(
                        &self.thinking_level_map,
                        ThinkingLevel::Off,
                    ) {
                        MapResult::Mapped(s) => s,
                        _ => "none".into(),
                    };
                    obj.insert("thinking".into(), json!(off));
                }
            }
            ThinkingFormat::Openai => {
                if let Some(e) = effort_ref {
                    if self.supports_reasoning_effort {
                        obj.insert("reasoning_effort".into(), json!(e));
                    }
                } else if self.supports_reasoning_effort {
                    // Explicit off mapping via thinkingLevelMap.off = "…"
                    if let MapResult::Mapped(s) =
                        resolve_thinking_level_map(&self.thinking_level_map, ThinkingLevel::Off)
                    {
                        obj.insert("reasoning_effort".into(), json!(s));
                    }
                }
            }
        }
    }

    /// Apply OpenRouter / Vercel routing and zai tool_stream extras.
    pub fn apply_routing_and_extras(&self, body: &mut Value, base_url: &str) {
        let Some(obj) = body.as_object_mut() else {
            return;
        };
        if let Some(routing) = &self.open_router_routing {
            if !routing.is_null()
                && routing
                    .as_object()
                    .map(|o| !o.is_empty())
                    .unwrap_or(true)
            {
                obj.insert("provider".into(), routing.clone());
            }
        }
        if base_url.contains("ai-gateway.vercel.sh") {
            if let Some(routing) = &self.vercel_gateway_routing {
                if let Some(r) = routing.as_object() {
                    let mut gateway = Map::new();
                    if let Some(only) = r.get("only") {
                        gateway.insert("only".into(), only.clone());
                    }
                    if let Some(order) = r.get("order") {
                        gateway.insert("order".into(), order.clone());
                    }
                    if !gateway.is_empty() {
                        obj.insert(
                            "providerOptions".into(),
                            json!({ "gateway": gateway }),
                        );
                    }
                }
            }
        }
        if self.zai_tool_stream {
            obj.insert("tool_stream".into(), json!(true));
        }
    }

    /// Optionally set max tokens using the configured field name.
    pub fn apply_max_tokens(&self, body: &mut Value, max_tokens: Option<u32>) {
        let Some(n) = max_tokens else {
            return;
        };
        let Some(obj) = body.as_object_mut() else {
            return;
        };
        obj.insert(self.max_tokens_field.as_str().to_string(), json!(n));
    }
}

fn build_chat_template_kwargs(template: &Option<Value>, effort: Option<&str>) -> Option<Value> {
    let tmpl = template.as_ref()?.as_object()?;
    let mut out = Map::new();
    for (key, value) in tmpl {
        if let Some(resolved) = resolve_chat_template_value(value, effort) {
            out.insert(key.clone(), resolved);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

fn resolve_chat_template_value(value: &Value, effort: Option<&str>) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if let Some(var) = map.get("$var").and_then(|v| v.as_str()) {
                let omit_when_off = map
                    .get("omitWhenOff")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if effort.is_none() && omit_when_off {
                    return None;
                }
                match var {
                    "thinking.enabled" => Some(json!(effort.is_some())),
                    "thinking.effort" => effort.map(|e| json!(e)),
                    _ => None,
                }
            } else {
                // Nested object: resolve recursively.
                let mut nested = Map::new();
                for (k, v) in map {
                    if let Some(r) = resolve_chat_template_value(v, effort) {
                        nested.insert(k.clone(), r);
                    }
                }
                Some(Value::Object(nested))
            }
        }
        other => Some(other.clone()),
    }
}

// ── Anthropic Messages compat (subset) ──────────────────────────────────────

/// Partial Anthropic Messages compatibility overrides (Pi `AnthropicMessagesCompat`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnthropicMessagesCompat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_eager_tool_input_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_cache_control_on_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_empty_signature: Option<bool>,
}

impl AnthropicMessagesCompat {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn merge_override(&self, over: &Self) -> Self {
        Self {
            supports_eager_tool_input_streaming: over
                .supports_eager_tool_input_streaming
                .or(self.supports_eager_tool_input_streaming),
            supports_long_cache_retention: over
                .supports_long_cache_retention
                .or(self.supports_long_cache_retention),
            send_session_affinity_headers: over
                .send_session_affinity_headers
                .or(self.send_session_affinity_headers),
            supports_cache_control_on_tools: over
                .supports_cache_control_on_tools
                .or(self.supports_cache_control_on_tools),
            supports_temperature: over.supports_temperature.or(self.supports_temperature),
            force_adaptive_thinking: over
                .force_adaptive_thinking
                .or(self.force_adaptive_thinking),
            allow_empty_signature: over.allow_empty_signature.or(self.allow_empty_signature),
        }
    }

    pub fn resolve(&self) -> ResolvedAnthropicCompat {
        ResolvedAnthropicCompat {
            supports_eager_tool_input_streaming: self
                .supports_eager_tool_input_streaming
                .unwrap_or(true),
            supports_long_cache_retention: self.supports_long_cache_retention.unwrap_or(true),
            send_session_affinity_headers: self.send_session_affinity_headers.unwrap_or(false),
            supports_cache_control_on_tools: self.supports_cache_control_on_tools.unwrap_or(true),
            supports_temperature: self.supports_temperature.unwrap_or(true),
            force_adaptive_thinking: self.force_adaptive_thinking.unwrap_or(false),
            allow_empty_signature: self.allow_empty_signature.unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAnthropicCompat {
    pub supports_eager_tool_input_streaming: bool,
    pub supports_long_cache_retention: bool,
    pub send_session_affinity_headers: bool,
    pub supports_cache_control_on_tools: bool,
    pub supports_temperature: bool,
    pub force_adaptive_thinking: bool,
    pub allow_empty_signature: bool,
}

impl Default for ResolvedAnthropicCompat {
    fn default() -> Self {
        AnthropicMessagesCompat::default().resolve()
    }
}

/// Unified on-disk compat blob: OpenAI + Anthropic fields may coexist; each
/// provider uses the subset it understands.
///
/// Shared field names (`supportsLongCacheRetention`, `sendSessionAffinityHeaders`)
/// live only on the OpenAI side to avoid `#[serde(flatten)]` collisions; Anthropic
/// resolve falls back to those OpenAI fields when the Anthropic-specific option is
/// unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompatConfig {
    #[serde(flatten)]
    pub openai: OpenAiCompletionsCompat,
    /// Anthropic-only fields (no overlap with OpenAI camelCase names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_eager_tool_input_streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_cache_control_on_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_empty_signature: Option<bool>,
}

impl CompatConfig {
    pub fn is_empty(&self) -> bool {
        self.openai.is_empty()
            && self.supports_eager_tool_input_streaming.is_none()
            && self.supports_cache_control_on_tools.is_none()
            && self.supports_temperature.is_none()
            && self.force_adaptive_thinking.is_none()
            && self.allow_empty_signature.is_none()
    }

    /// Short summary for Settings UI (e.g. `devRole=false · format=openrouter`).
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.openai.supports_developer_role {
            parts.push(format!("devRole={v}"));
        }
        if let Some(v) = self.openai.supports_reasoning_effort {
            parts.push(format!("effort={v}"));
        }
        if let Some(f) = self.openai.thinking_format {
            parts.push(format!("format={}", f.as_str()));
        }
        if let Some(f) = self.openai.max_tokens_field {
            parts.push(format!("maxTok={}", f.as_str()));
        }
        if let Some(true) = self.force_adaptive_thinking {
            parts.push("adaptiveThink".into());
        }
        if parts.is_empty() {
            "overrides set".into()
        } else {
            parts.join(" · ")
        }
    }

    /// Display a tri-state bool field: `auto` / `true` / `false`.
    pub fn get_tri(&self, key: &str) -> &'static str {
        match self.get_bool(key) {
            None => "auto",
            Some(true) => "true",
            Some(false) => "false",
        }
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match normalize_compat_key(key).as_str() {
            "supports_developer_role" => self.openai.supports_developer_role,
            "supports_reasoning_effort" => self.openai.supports_reasoning_effort,
            "supports_usage_in_streaming" => self.openai.supports_usage_in_streaming,
            "supports_store" => self.openai.supports_store,
            "requires_tool_result_name" => self.openai.requires_tool_result_name,
            "requires_assistant_after_tool_result" => {
                self.openai.requires_assistant_after_tool_result
            }
            "requires_thinking_as_text" => self.openai.requires_thinking_as_text,
            "requires_reasoning_content_on_assistant_messages" => {
                self.openai.requires_reasoning_content_on_assistant_messages
            }
            "supports_strict_mode" => self.openai.supports_strict_mode,
            "zai_tool_stream" => self.openai.zai_tool_stream,
            "send_session_affinity_headers" => self.openai.send_session_affinity_headers,
            "supports_long_cache_retention" => self.openai.supports_long_cache_retention,
            "supports_eager_tool_input_streaming" => self.supports_eager_tool_input_streaming,
            "supports_cache_control_on_tools" => self.supports_cache_control_on_tools,
            "supports_temperature" => self.supports_temperature,
            "force_adaptive_thinking" => self.force_adaptive_thinking,
            "allow_empty_signature" => self.allow_empty_signature,
            _ => None,
        }
    }

    /// Set a tri-state bool (`true`/`false`/`auto`/empty clears).
    pub fn set_tri(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = parse_tri_bool(value)?;
        self.set_bool(key, v)
    }

    pub fn set_bool(&mut self, key: &str, value: Option<bool>) -> Result<(), String> {
        match normalize_compat_key(key).as_str() {
            "supports_developer_role" => self.openai.supports_developer_role = value,
            "supports_reasoning_effort" => self.openai.supports_reasoning_effort = value,
            "supports_usage_in_streaming" => self.openai.supports_usage_in_streaming = value,
            "supports_store" => self.openai.supports_store = value,
            "requires_tool_result_name" => self.openai.requires_tool_result_name = value,
            "requires_assistant_after_tool_result" => {
                self.openai.requires_assistant_after_tool_result = value
            }
            "requires_thinking_as_text" => self.openai.requires_thinking_as_text = value,
            "requires_reasoning_content_on_assistant_messages" => {
                self.openai.requires_reasoning_content_on_assistant_messages = value
            }
            "supports_strict_mode" => self.openai.supports_strict_mode = value,
            "zai_tool_stream" => self.openai.zai_tool_stream = value,
            "send_session_affinity_headers" => self.openai.send_session_affinity_headers = value,
            "supports_long_cache_retention" => self.openai.supports_long_cache_retention = value,
            "supports_eager_tool_input_streaming" => {
                self.supports_eager_tool_input_streaming = value
            }
            "supports_cache_control_on_tools" => self.supports_cache_control_on_tools = value,
            "supports_temperature" => self.supports_temperature = value,
            "force_adaptive_thinking" => self.force_adaptive_thinking = value,
            "allow_empty_signature" => self.allow_empty_signature = value,
            other => {
                return Err(format!(
                    "unknown compat bool `{other}` · try supportsDeveloperRole, supportsReasoningEffort, …"
                ));
            }
        }
        Ok(())
    }

    pub fn thinking_format_display(&self) -> String {
        self.openai
            .thinking_format
            .map(|f| f.as_str().to_string())
            .unwrap_or_else(|| "auto".into())
    }

    pub fn set_thinking_format(&mut self, value: &str) -> Result<(), String> {
        let v = value.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("default") {
            self.openai.thinking_format = None;
            return Ok(());
        }
        self.openai.thinking_format = Some(
            ThinkingFormat::parse(v).ok_or_else(|| {
                format!(
                    "invalid thinkingFormat `{v}` · openai|openrouter|deepseek|together|zai|qwen|…"
                )
            })?,
        );
        Ok(())
    }

    pub fn max_tokens_field_display(&self) -> String {
        self.openai
            .max_tokens_field
            .map(|f| f.as_str().to_string())
            .unwrap_or_else(|| "auto".into())
    }

    pub fn set_max_tokens_field(&mut self, value: &str) -> Result<(), String> {
        let v = value.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("default") {
            self.openai.max_tokens_field = None;
            return Ok(());
        }
        self.openai.max_tokens_field = Some(MaxTokensField::parse(v).ok_or_else(|| {
            format!("invalid maxTokensField `{v}` · max_tokens | max_completion_tokens | auto")
        })?);
        Ok(())
    }

    /// Cycle a tri-state bool: auto → true → false → auto.
    pub fn cycle_tri(&mut self, key: &str) -> Result<&'static str, String> {
        let next = match self.get_bool(key) {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
        self.set_bool(key, next)?;
        Ok(match next {
            None => "auto",
            Some(true) => "true",
            Some(false) => "false",
        })
    }

    pub fn anthropic(&self) -> AnthropicMessagesCompat {
        AnthropicMessagesCompat {
            supports_eager_tool_input_streaming: self.supports_eager_tool_input_streaming,
            // Shared names: reuse OpenAI-side options when present.
            supports_long_cache_retention: self.openai.supports_long_cache_retention,
            send_session_affinity_headers: self.openai.send_session_affinity_headers,
            supports_cache_control_on_tools: self.supports_cache_control_on_tools,
            supports_temperature: self.supports_temperature,
            force_adaptive_thinking: self.force_adaptive_thinking,
            allow_empty_signature: self.allow_empty_signature,
        }
    }

    pub fn merge_override(&self, over: &Self) -> Self {
        Self {
            openai: self.openai.merge_override(&over.openai),
            supports_eager_tool_input_streaming: over
                .supports_eager_tool_input_streaming
                .or(self.supports_eager_tool_input_streaming),
            supports_cache_control_on_tools: over
                .supports_cache_control_on_tools
                .or(self.supports_cache_control_on_tools),
            supports_temperature: over.supports_temperature.or(self.supports_temperature),
            force_adaptive_thinking: over
                .force_adaptive_thinking
                .or(self.force_adaptive_thinking),
            allow_empty_signature: over.allow_empty_signature.or(self.allow_empty_signature),
        }
    }
}

/// Normalize UI / models.json key names to snake_case identifiers.
pub fn normalize_compat_key(key: &str) -> String {
    let k = key.trim().trim_start_matches("compat.").trim_start_matches("compat_");
    // camelCase → snake_case
    let mut out = String::new();
    for (i, c) in k.chars().enumerate() {
        if c == '-' || c == '.' {
            out.push('_');
            continue;
        }
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_tri_bool(value: &str) -> Result<Option<bool>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" | "default" | "unset" | "inherit" | "clear" | "-" => Ok(None),
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        other => Err(format!("expected true|false|auto, got `{other}`")),
    }
}

/// Known editable bool keys for Settings UI (display label, storage key).
pub const COMPAT_BOOL_FIELDS: &[(&str, &str)] = &[
    ("supportsDeveloperRole", "supports_developer_role"),
    ("supportsReasoningEffort", "supports_reasoning_effort"),
    ("supportsUsageInStreaming", "supports_usage_in_streaming"),
    ("supportsStore", "supports_store"),
    ("requiresToolResultName", "requires_tool_result_name"),
    (
        "requiresAssistantAfterToolResult",
        "requires_assistant_after_tool_result",
    ),
    ("requiresThinkingAsText", "requires_thinking_as_text"),
    (
        "requiresReasoningContentOnAssistantMessages",
        "requires_reasoning_content_on_assistant_messages",
    ),
    ("supportsStrictMode", "supports_strict_mode"),
    ("forceAdaptiveThinking", "force_adaptive_thinking"),
    ("allowEmptySignature", "allow_empty_signature"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_ollama_disables_developer_and_effort() {
        let c = OpenAiCompletionsCompat::detect("ollama", "http://127.0.0.1:11434/v1", "llama3");
        let r = c.resolve("ollama", "http://127.0.0.1:11434/v1", "llama3");
        assert!(!r.supports_developer_role);
        assert!(!r.supports_reasoning_effort);
        assert_eq!(r.max_tokens_field, MaxTokensField::MaxTokens);
        assert_eq!(r.system_role(true), "system");
    }

    #[test]
    fn detect_openrouter_thinking_format() {
        let r = OpenAiCompletionsCompat::default().resolve(
            "openrouter",
            "https://openrouter.ai/api/v1",
            "anthropic/claude-sonnet-4",
        );
        assert_eq!(r.thinking_format, ThinkingFormat::Openrouter);
        assert!(r.supports_developer_role); // anthropic/ prefix
        assert_eq!(
            r.cache_control_format.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn detect_deepseek_requires_reasoning_content() {
        let r = OpenAiCompletionsCompat::default().resolve(
            "deepseek",
            "https://api.deepseek.com",
            "deepseek-reasoner",
        );
        assert!(r.requires_reasoning_content_on_assistant_messages);
        assert_eq!(r.thinking_format, ThinkingFormat::Deepseek);
    }

    #[test]
    fn model_override_wins() {
        let provider = OpenAiCompletionsCompat {
            supports_developer_role: Some(true),
            thinking_format: Some(ThinkingFormat::Openai),
            ..Default::default()
        };
        let model = OpenAiCompletionsCompat {
            supports_developer_role: Some(false),
            ..Default::default()
        };
        let merged = provider.merge_override(&model);
        assert_eq!(merged.supports_developer_role, Some(false));
        assert_eq!(merged.thinking_format, Some(ThinkingFormat::Openai));
    }

    #[test]
    fn apply_openrouter_thinking() {
        let r = OpenAiCompletionsCompat {
            thinking_format: Some(ThinkingFormat::Openrouter),
            supports_reasoning_effort: Some(true),
            ..Default::default()
        }
        .resolve("openrouter", "https://openrouter.ai/api/v1", "x");
        let mut body = json!({"model": "x"});
        r.apply_thinking(&mut body, ThinkingLevel::High, true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["include_reasoning"], true);
    }

    #[test]
    fn apply_openai_thinking_gated_by_supports_effort() {
        let r = OpenAiCompletionsCompat {
            thinking_format: Some(ThinkingFormat::Openai),
            supports_reasoning_effort: Some(false),
            ..Default::default()
        }
        .resolve("ollama", "http://localhost:11434/v1", "x");
        let mut body = json!({"model": "x"});
        r.apply_thinking(&mut body, ThinkingLevel::Medium, true);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn routing_openrouter() {
        let r = OpenAiCompletionsCompat {
            open_router_routing: Some(json!({"only": ["anthropic"]})),
            ..Default::default()
        }
        .resolve("openrouter", "https://openrouter.ai/api/v1", "x");
        let mut body = json!({"model": "x"});
        r.apply_routing_and_extras(&mut body, "https://openrouter.ai/api/v1");
        assert_eq!(body["provider"]["only"][0], "anthropic");
    }

    #[test]
    fn thinking_level_map_parse_and_apply() {
        let map = parse_thinking_level_map("high=max,xhigh=null,low=null").unwrap();
        assert_eq!(
            resolve_thinking_level_map(&map, ThinkingLevel::High),
            MapResult::Mapped("max".into())
        );
        assert_eq!(
            resolve_thinking_level_map(&map, ThinkingLevel::Low),
            MapResult::Unsupported
        );
        assert_eq!(
            resolve_thinking_level_map(&map, ThinkingLevel::Medium),
            MapResult::Default
        );

        let r = OpenAiCompletionsCompat {
            thinking_format: Some(ThinkingFormat::Openai),
            supports_reasoning_effort: Some(true),
            ..Default::default()
        }
        .resolve("openai", "https://api.openai.com/v1", "x")
        .with_thinking_level_map(map);
        let mut body = json!({"model": "x"});
        r.apply_thinking(&mut body, ThinkingLevel::High, true);
        assert_eq!(body["reasoning_effort"], "max");
        let mut body2 = json!({"model": "x"});
        r.apply_thinking(&mut body2, ThinkingLevel::Low, true);
        assert!(body2.get("reasoning_effort").is_none());
    }

    #[test]
    fn detect_groq_and_lmstudio() {
        let groq = OpenAiCompletionsCompat::default().resolve(
            "groq",
            "https://api.groq.com/openai/v1",
            "llama-3.1",
        );
        assert!(!groq.supports_developer_role);
        assert_eq!(groq.max_tokens_field, MaxTokensField::MaxTokens);

        let lms = OpenAiCompletionsCompat::default().resolve(
            "lmstudio",
            "http://localhost:1234/v1",
            "local",
        );
        assert!(!lms.supports_developer_role);
        assert!(!lms.supports_reasoning_effort);
    }

    #[test]
    fn compat_cycle_tri() {
        let mut c = CompatConfig::default();
        assert_eq!(c.cycle_tri("supportsDeveloperRole").unwrap(), "true");
        assert_eq!(c.cycle_tri("supportsDeveloperRole").unwrap(), "false");
        assert_eq!(c.cycle_tri("supportsDeveloperRole").unwrap(), "auto");
    }

    #[test]
    fn serde_roundtrip_compat_config() {
        let raw = r#"{
            "supportsDeveloperRole": false,
            "supportsReasoningEffort": false,
            "thinkingFormat": "openrouter",
            "openRouterRouting": { "only": ["amazon-bedrock"] },
            "forceAdaptiveThinking": true
        }"#;
        let c: CompatConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(c.openai.supports_developer_role, Some(false));
        assert_eq!(c.openai.thinking_format, Some(ThinkingFormat::Openrouter));
        assert_eq!(c.force_adaptive_thinking, Some(true));
        assert_eq!(c.anthropic().force_adaptive_thinking, Some(true));
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["supportsDeveloperRole"], false);
        assert_eq!(back["forceAdaptiveThinking"], true);
    }
}
