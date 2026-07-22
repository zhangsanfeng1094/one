//! Shared argument helpers for coding tools (Claude Code / OpenCode / Pi compatibility).

use serde_json::Value;

/// Resolve a filesystem path from tool args.
///
/// Accepts One `path`, Claude `file_path`, or OpenCode `filePath`.
pub fn path_arg(args: &Value) -> Option<&str> {
    string_arg_nonempty(args, &["path", "file_path", "filePath"])
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

/// JSON Schema fragment: `path` + Claude / OpenCode aliases.
pub fn path_properties(path_description: &str) -> Value {
    serde_json::json!({
        "path": {
            "type": "string",
            "description": path_description
        },
        "file_path": {
            "type": "string",
            "description": "Alias for `path` (Claude Code compatibility)"
        },
        "filePath": {
            "type": "string",
            "description": "Alias for `path` (OpenCode compatibility)"
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
    fn path_prefers_path_then_aliases() {
        assert_eq!(
            path_arg(&json!({"path": "a.rs", "file_path": "b.rs"})),
            Some("a.rs")
        );
        assert_eq!(path_arg(&json!({"file_path": "b.rs"})), Some("b.rs"));
        assert_eq!(path_arg(&json!({"filePath": "c.rs"})), Some("c.rs"));
        assert_eq!(path_arg(&json!({"path": "  "})), None);
        assert_eq!(path_arg(&json!({})), None);
    }

    #[test]
    fn edit_string_aliases() {
        assert_eq!(
            old_string_arg(&json!({"oldString": "x"})),
            Some("x")
        );
        assert_eq!(old_string_arg(&json!({"oldText": "y"})), Some("y"));
        assert_eq!(
            new_string_arg(&json!({"newString": "z"})),
            Some("z")
        );
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
            bool_arg_names(&json!({"replace_all": false}), &["replace_all", "replaceAll"]),
            Some(false)
        );
    }
}
