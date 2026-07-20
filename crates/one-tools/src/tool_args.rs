//! Shared argument helpers for coding tools (Claude Code compatibility).

use serde_json::Value;

/// Resolve a filesystem path from tool args.
///
/// Accepts One-style `path` or Claude Code-style `file_path` (either works).
pub fn path_arg(args: &Value) -> Option<&str> {
    args.get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            args.get("file_path")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
}

/// JSON Schema fragment: `path` + Claude alias `file_path`.
pub fn path_properties(path_description: &str) -> Value {
    serde_json::json!({
        "path": {
            "type": "string",
            "description": path_description
        },
        "file_path": {
            "type": "string",
            "description": "Alias for `path` (Claude Code compatibility)"
        }
    })
}

/// Optional boolean: prefer `name`, fall back to `alias`.
pub fn bool_arg(args: &Value, name: &str, alias: Option<&str>) -> Option<bool> {
    args.get(name)
        .and_then(|v| v.as_bool())
        .or_else(|| alias.and_then(|a| args.get(a).and_then(|v| v.as_bool())))
}

/// Optional u64 integer arg.
pub fn u64_arg(args: &Value, name: &str) -> Option<u64> {
    args.get(name).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_i64().map(|n| n.max(0) as u64))
            .or_else(|| v.as_f64().map(|n| n.max(0.0) as u64))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_prefers_path_then_file_path() {
        assert_eq!(
            path_arg(&json!({"path": "a.rs", "file_path": "b.rs"})),
            Some("a.rs")
        );
        assert_eq!(path_arg(&json!({"file_path": "b.rs"})), Some("b.rs"));
        assert_eq!(path_arg(&json!({"path": "  "})), None);
        assert_eq!(path_arg(&json!({})), None);
    }
}
