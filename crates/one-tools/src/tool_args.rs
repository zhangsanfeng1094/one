//! Shared argument helpers for coding tools (Claude Code / OpenCode / Pi compatibility).
//!
//! **Schema vs runtime:** JSON Schema should advertise only the canonical key
//! (`path`, `timeout_secs`, …). Runtime still accepts Claude/OpenCode aliases so
//! old transcripts and foreign models keep working. If the model sends more than
//! one alias and the values disagree, we refuse — guessing the wrong path is
//! how traces turned a good `file_path` into an ENOENT / outside-workspace write.

use serde_json::Value;

use one_core::error::Result;
use one_core::tool::invalid_args;

/// Runtime-accepted path keys. Only [`path_properties`] is shown to the model.
pub const PATH_ARG_KEYS: &[&str] = &["path", "file_path", "filePath"];

/// Resolve a filesystem path from tool args.
///
/// Accepts One `path`, Claude `file_path`, or OpenCode `filePath`. Empty strings
/// are ignored. Distinct non-empty values are a conflict (`Err`), not a silent
/// preference for `path`.
pub fn path_arg(args: &Value) -> std::result::Result<Option<&str>, String> {
    let mut unique: Vec<(&str, &str)> = Vec::new();
    for key in PATH_ARG_KEYS {
        let Some(value) = args
            .get(*key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if !unique.iter().any(|(_, existing)| *existing == value) {
            unique.push((*key, value));
        }
    }
    match unique.as_slice() {
        [] => Ok(None),
        [(_, value)] => Ok(Some(*value)),
        many => {
            let detail = many
                .iter()
                .map(|(k, v)| format!("{k}={v:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "conflicting path aliases ({detail}). Pass only `path` — do not also send \
                 `file_path` / `filePath` with a different value."
            ))
        }
    }
}

/// [`path_arg`] mapped to a tool `invalid_args` error.
pub fn path_arg_for_tool<'a>(args: &'a Value, tool: &str, missing: &str) -> Result<&'a str> {
    match path_arg(args) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(invalid_args(tool, missing)),
        Err(msg) => Err(invalid_args(tool, msg)),
    }
}

/// [`path_arg`] with a default when no alias is present. Conflicts still fail.
pub fn path_arg_or<'a>(args: &'a Value, default: &'a str) -> std::result::Result<&'a str, String> {
    Ok(path_arg(args)?.unwrap_or(default))
}

/// First present string among `names` (may be empty).
pub fn string_arg<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    for name in names {
        if let Some(s) = args.get(*name).and_then(|v| v.as_str()) {
            return Some(s);
        }
    }
    None
}

/// First present non-empty trimmed string among `names`.
pub fn string_arg_nonempty<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    for name in names {
        if let Some(s) = args
            .get(*name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s);
        }
    }
    None
}

/// Edit `old_string` aliases: Claude snake_case, OpenCode camelCase, Pi `oldText`.
pub fn old_string_arg(args: &Value) -> Option<&str> {
    string_arg(args, &["old_string", "oldString", "oldText"])
}

/// Edit `new_string` aliases: Claude / OpenCode / Pi.
pub fn new_string_arg(args: &Value) -> Option<&str> {
    string_arg(args, &["new_string", "newString", "newText"])
}

/// JSON Schema fragment: **only** canonical `path`.
///
/// Callers should list `"path"` in the tool's `required` array so models always
/// emit a path (Grok Build / Claude-style). Runtime still accepts `file_path` /
/// `filePath` via [`path_arg`] when a model uses those aliases *instead of*
/// `path`. Advertising the aliases in schema made gpt-5.x fill all three and
/// then disagree.
pub fn path_properties(path_description: &str) -> Value {
    serde_json::json!({
        "path": {
            "type": "string",
            "description": path_description
        }
    })
}

/// Optional boolean: try each name in order.
pub fn bool_arg(args: &Value, name: &str, alias: Option<&str>) -> Option<bool> {
    let mut names = vec![name];
    if let Some(a) = alias {
        names.push(a);
    }
    bool_arg_names(args, &names)
}

/// Optional boolean among several names (e.g. `replace_all` / `replaceAll`).
pub fn bool_arg_names(args: &Value, names: &[&str]) -> Option<bool> {
    for name in names {
        if let Some(b) = args.get(*name).and_then(|v| v.as_bool()) {
            return Some(b);
        }
    }
    None
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
    fn path_accepts_any_single_alias() {
        assert_eq!(
            path_arg(&json!({"file_path": "b.rs"})).unwrap(),
            Some("b.rs")
        );
        assert_eq!(
            path_arg(&json!({"filePath": "c.rs"})).unwrap(),
            Some("c.rs")
        );
        assert_eq!(path_arg(&json!({"path": "a.rs"})).unwrap(), Some("a.rs"));
        assert_eq!(path_arg(&json!({"path": "  "})).unwrap(), None);
        assert_eq!(path_arg(&json!({})).unwrap(), None);
    }

    #[test]
    fn path_identical_aliases_are_ok() {
        assert_eq!(
            path_arg(&json!({"path": "a.rs", "file_path": "a.rs", "filePath": "a.rs"})).unwrap(),
            Some("a.rs")
        );
        // empty aliases are ignored, not a conflict
        assert_eq!(
            path_arg(&json!({"path": "src/", "file_path": ""})).unwrap(),
            Some("src/")
        );
    }

    #[test]
    fn path_rejects_disagreeing_aliases() {
        let err = path_arg(&json!({
            "path": "/home/fxh/tools/one-mcp/src/manager.rs",
            "file_path": "/home/fxh/tools/one/crates/one-mcp/src/manager.rs",
            "filePath": "/home/fxh/tools/one/crates/one-mcp/src/manager.rs"
        }))
        .unwrap_err();
        assert!(err.contains("conflicting path aliases"), "{err}");
        assert!(err.contains("path="), "{err}");
        assert!(err.contains("file_path="), "{err}");
    }

    #[test]
    fn path_properties_exposes_only_canonical_key() {
        let props = path_properties("Required file path.");
        let obj = props.as_object().expect("object");
        assert!(obj.contains_key("path"));
        assert!(
            !obj.contains_key("file_path"),
            "do not advertise aliases: {obj:?}"
        );
        assert!(
            !obj.contains_key("filePath"),
            "do not advertise aliases: {obj:?}"
        );
    }

    #[test]
    fn edit_string_aliases() {
        assert_eq!(old_string_arg(&json!({"oldString": "x"})), Some("x"));
        assert_eq!(old_string_arg(&json!({"oldText": "y"})), Some("y"));
        assert_eq!(new_string_arg(&json!({"newString": "z"})), Some("z"));
        // empty is still present (caller decides validity)
        assert_eq!(old_string_arg(&json!({"old_string": ""})), Some(""));
    }

    #[test]
    fn replace_all_aliases() {
        assert_eq!(
            bool_arg_names(&json!({"replaceAll": true}), &["replace_all", "replaceAll"]),
            Some(true)
        );
        assert_eq!(
            bool_arg_names(
                &json!({"replace_all": false}),
                &["replace_all", "replaceAll"]
            ),
            Some(false)
        );
    }
}
