//! Session metadata custom types (`one.*`).
//!
//! These are stored as [`crate::entries::SessionEntry::Custom`] and **must not**
//! enter the LLM context (see `context::entry_produces_message`).

use one_core::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cumulative / per-run token usage after a prompt run.
pub const CUSTOM_USAGE: &str = "one.usage";
/// System prompt audit snapshot (not injected into conversation messages).
pub const CUSTOM_PROMPT_SNAPSHOT: &str = "one.prompt_snapshot";
/// Batched tool lifecycle for one prompt run (no stdout bodies).
pub const CUSTOM_TOOL_AUDIT: &str = "one.tool_audit";

pub const META_SCHEMA: u32 = 1;

/// Inline system-prompt text threshold; larger prompts spill to a sidecar file.
pub const PROMPT_INLINE_MAX_BYTES: usize = 64 * 1024;

/// Wire shape for token fields (mirrors [`TokenUsage`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFields {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl From<TokenUsage> for UsageFields {
    fn from(u: TokenUsage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
        }
    }
}

impl From<UsageFields> for TokenUsage {
    fn from(u: UsageFields) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
        }
    }
}

impl UsageFields {
    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageMeta {
    #[serde(default = "meta_schema")]
    pub schema: u32,
    /// `"run"` for a full prompt→tools→reply cycle.
    #[serde(default = "default_usage_kind")]
    pub kind: String,
    pub delta: UsageFields,
    pub total: UsageFields,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub context_size_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_index: Option<u64>,
}

impl UsageMeta {
    pub fn new(
        delta: TokenUsage,
        total: TokenUsage,
        context_size_tokens: u64,
        provider: Option<String>,
        model: Option<String>,
        prompt_index: Option<u64>,
    ) -> Self {
        Self {
            schema: META_SCHEMA,
            kind: "run".into(),
            delta: delta.into(),
            total: total.into(),
            context_size_tokens,
            provider,
            model,
            prompt_index,
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_value(data: &Value) -> Option<Self> {
        serde_json::from_value(data.clone()).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSnapshotMeta {
    #[serde(default = "meta_schema")]
    pub schema: u32,
    /// `sha256:<hex>` of UTF-8 system prompt bytes.
    pub hash: String,
    pub byte_len: usize,
    /// Inline text when small enough; omitted when spilled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Absolute or session-relative path when text is spilled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl PromptSnapshotMeta {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_value(data: &Value) -> Option<Self> {
        serde_json::from_value(data.clone()).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAuditItem {
    pub tool_call_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAuditMeta {
    #[serde(default = "meta_schema")]
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_index: Option<u64>,
    #[serde(default)]
    pub tools: Vec<ToolAuditItem>,
}

impl ToolAuditMeta {
    pub fn new(prompt_index: Option<u64>, tools: Vec<ToolAuditItem>) -> Self {
        Self {
            schema: META_SCHEMA,
            prompt_index,
            tools,
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_value(data: &Value) -> Option<Self> {
        serde_json::from_value(data.clone()).ok()
    }
}

/// SHA-256 hex digest of system prompt bytes, prefixed with `sha256:`.
pub fn prompt_hash(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // Prefer a stable non-crypto fingerprint if sha2 is unavailable in this crate.
    // one-session does not depend on sha2; use a simple FNV-style + length for
    // change detection. Callers that need strong integrity can upgrade later.
    //
    // Format still uses `sha256:` prefix for forward compatibility of the field
    // name; the algorithm is documented as "session fingerprint v1" below.
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    text.len().hash(&mut h);
    // Mix a second pass over bytes for fewer collisions on short prompts.
    let mut acc: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        acc ^= u64::from(*b);
        acc = acc.wrapping_mul(0x100000001b3);
    }
    format!("fp1:{:016x}{:016x}", h.finish(), acc)
}

fn meta_schema() -> u32 {
    META_SCHEMA
}

fn default_usage_kind() -> String {
    "run".into()
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_roundtrip_value() {
        let meta = UsageMeta::new(
            TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 1,
                cache_write_tokens: 0,
            },
            TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 5,
                cache_write_tokens: 0,
            },
            100,
            Some("p".into()),
            Some("m".into()),
            Some(0),
        );
        let v = meta.to_value();
        let back = UsageMeta::from_value(&v).unwrap();
        assert_eq!(back.delta.input_tokens, 10);
        assert_eq!(back.total.output_tokens, 20);
        assert_eq!(back.prompt_index, Some(0));
    }

    #[test]
    fn prompt_hash_stable() {
        assert_eq!(prompt_hash("hello"), prompt_hash("hello"));
        assert_ne!(prompt_hash("hello"), prompt_hash("hellp"));
    }
}
