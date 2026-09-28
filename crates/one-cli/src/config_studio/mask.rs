//! Credential masking for the config studio.
//!
//! Two complementary mechanisms keep secrets out of the browser while still
//! allowing precise edits:
//!
//! 1. **Value masking** ([`mask_value`]) replaces sensitive *values* with
//!    [`REDACTED`] inside a parsed JSON tree. Form edits are applied to the
//!    masked tree and merged back with [`restore_secrets`], which copies the
//!    real value back for every field the user left at the sentinel. This makes
//!    "保持 / 替换 / 清除" explicit rather than implicit.
//! 2. **Text masking** ([`mask_text`]) rewrites raw source without reformatting,
//!    so a locked source view still shows the file's real layout. It also works
//!    on files that fail to parse.

use std::collections::BTreeSet;

use serde_json::Value;

/// Sentinel written in place of a secret value.
///
/// Chosen to be visually obvious and effectively impossible to collide with a
/// real credential.
pub const REDACTED: &str = "***REDACTED***";

/// Key names that are *never* secrets even though they contain a secret-ish word.
const SAFE_KEYS: &[&str] = &[
    "maxtokens",
    "maxoutputtokens",
    "maxinputtokens",
    "maxtokenbudget",
    "tokenbudget",
    "tokencount",
    "tokenlimit",
    "tokens",
    "tokenizer",
    "secretstore",
    "keymap",
    "keybinding",
    "keywords",
    "monkey",
    "keys",
    "keyid",
    "secretname",
    "tokenprefix",
];

/// Keys that are always treated as secret when they appear verbatim.
const SENSITIVE_KEYS: &[&str] = &[
    "apikey",
    "apikeyid",
    "apitoken",
    "key",
    "secret",
    "secretkey",
    "clientsecret",
    "password",
    "passwd",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "bearer",
    "privatekey",
    "accesskey",
    "accesskeyid",
    "sessionkey",
    "sessiontoken",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "authtoken",
];

/// Suffixes that make a key secret regardless of its prefix.
const SENSITIVE_SUFFIXES: &[&str] = &[
    "apikey",
    "apikeys",
    "secret",
    "secrets",
    "password",
    "passwd",
    "token",
    "credentials",
    "privatekey",
];

/// Map keys whose values receive the extra token-shape heuristic.
const MAP_KEYS: &[&str] = &["env", "envvars", "environment", "headers", "header"];

/// Normalize a key for comparison: lowercase, drop separators.
pub fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a key name marks its value as a credential.
pub fn is_sensitive_key(key: &str) -> bool {
    let n = normalize_key(key);
    if n.is_empty() || SAFE_KEYS.contains(&n.as_str()) {
        return false;
    }
    if SENSITIVE_KEYS.contains(&n.as_str()) {
        return true;
    }
    SENSITIVE_SUFFIXES.iter().any(|s| n.ends_with(s))
}

/// Whether a raw value looks like a literal credential rather than a
/// configuration value or an `${ENV_VAR}` reference.
///
/// Used only for values inside `env` / `headers` maps, where key names are
/// environment-variable style (`FOO_SECRET_VALUE`) and therefore not reliably
/// classifiable by name alone.
pub fn looks_like_token(value: &str) -> bool {
    if value.is_empty() || is_env_reference(value) {
        return false;
    }
    const PREFIXES: [&str; 9] = [
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "eyJ",
    ];
    if PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    // Long, unbroken, mixed-class strings are almost certainly credentials;
    // paths and sentences are excluded by the separator/whitespace checks.
    value.len() >= 24
        && value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_ascii_alphabetic())
        && !value.chars().any(|c| c.is_whitespace())
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains(',')
}

/// Whether a value is an environment reference such as `${OPENAI_API_KEY}`.
///
/// References are configuration, not credentials, and are shown verbatim so the
/// user can see *which* variable supplies the secret.
pub fn is_env_reference(value: &str) -> bool {
    let trimmed = value.trim();
    if let Some(rest) = trimmed.strip_prefix("${") {
        return rest.contains('}');
    }
    if let Some(rest) = trimmed.strip_prefix('$') {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    false
}

/// Everything a masking pass learned, used by both mask entry points.
#[derive(Default)]
struct MaskSink {
    /// Dot paths whose value was replaced.
    paths: Vec<String>,
    /// The original literal values that were replaced. Text masking needs these
    /// because it cannot know which container it is inside.
    literals: BTreeSet<String>,
}

/// Mask sensitive values in a parsed JSON tree.
///
/// Returns the masked tree plus the dot paths that were masked, so the UI can
/// annotate those fields.
pub fn mask_value(value: &Value) -> (Value, Vec<String>) {
    let mut sink = MaskSink::default();
    let masked = mask_value_at(value, "", None, &mut sink);
    (masked, sink.paths)
}

/// Literal values that masking would replace, collected from a parsed tree.
///
/// Used by [`mask_text`] so a linewise rewrite reaches the same conclusions as
/// structural masking — importantly for `env` / `headers` maps, whose values are
/// secrets even when the key name says nothing (`FOO_SECRET_VALUE`).
fn sensitive_literals(value: &Value) -> BTreeSet<String> {
    let mut sink = MaskSink::default();
    let _ = mask_value_at(value, "", None, &mut sink);
    sink.literals
}

fn mask_value_at(value: &Value, path: &str, container: Option<&str>, sink: &mut MaskSink) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let in_secret_container = container
                    .map(|c| MAP_KEYS.contains(&normalize_key(c).as_str()))
                    .unwrap_or(false);

                let is_secret = if child.is_object() || child.is_array() {
                    false
                } else {
                    is_sensitive_key(key)
                        || (in_secret_container
                            && child.as_str().map(looks_like_token).unwrap_or(false))
                };

                if is_secret {
                    let real = child.as_str().unwrap_or_default();
                    if is_env_reference(real) {
                        // Show which variable supplies the secret.
                        out.insert(key.clone(), child.clone());
                    } else {
                        out.insert(key.clone(), Value::String(REDACTED.to_string()));
                        sink.paths.push(child_path.clone());
                        sink.literals.insert(real.to_string());
                    }
                } else {
                    out.insert(
                        key.clone(),
                        mask_value_at(child, &child_path, Some(key), sink),
                    );
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let child_path = format!("{path}[{i}]");
                    mask_value_at(item, &child_path, container, sink)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Copy real secret values back into a draft that still carries [`REDACTED`].
///
/// * A field left at the sentinel keeps its previous value ("保持").
/// * A field absent from the draft stays absent, deleting the stored secret ("清除").
/// * Any other value replaces the stored one ("替换").
pub fn restore_secrets(draft: &mut Value, existing: &Value) {
    restore_at(draft, existing, None);
}

fn restore_at(draft: &mut Value, existing: &Value, container: Option<&str>) {
    match draft {
        Value::Object(map) => {
            let existing_map = existing.as_object();
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let in_secret_container = container
                    .map(|c| MAP_KEYS.contains(&normalize_key(c).as_str()))
                    .unwrap_or(false);

                let current = map.get(&key).cloned().unwrap_or(Value::Null);
                match &current {
                    Value::String(s) if s == REDACTED => {
                        match existing_map.and_then(|m| m.get(&key)) {
                            Some(real) => {
                                map.insert(key.clone(), real.clone());
                            }
                            // Keep the sentinel. `dangling_sentinels` turns this
                            // into a 422 so a rename or missing origin cannot
                            // silently delete the credential.
                            None => {}
                        }
                    }
                    Value::Object(_) | Value::Array(_) => {
                        if let Some(child) = map.get_mut(&key) {
                            let base = existing_map
                                .and_then(|m| m.get(&key))
                                .cloned()
                                .unwrap_or(Value::Null);
                            restore_at(child, &base, Some(&key));
                        }
                    }
                    Value::String(s) if in_secret_container && looks_like_token(s) => {
                        // A literal that never went through masking (e.g. typed by
                        // hand) is treated as a deliberate replacement.
                        let _ = in_secret_container;
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            let existing_items = existing.as_array();
            for (i, item) in items.iter_mut().enumerate() {
                match item {
                    Value::String(s) if s == REDACTED => {
                        if let Some(real) = existing_items
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_str())
                        {
                            *s = real.to_string();
                        }
                    }
                    Value::Object(_) | Value::Array(_) => {
                        let base = existing_items
                            .and_then(|a| a.get(i))
                            .cloned()
                            .unwrap_or(Value::Null);
                        restore_at(item, &base, container);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Paths in `draft` that still hold the sentinel after a restore attempt.
///
/// A non-empty result means the client submitted a mask with nothing to restore
/// from, which would write the sentinel into the config file.
pub fn dangling_sentinels(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_sentinels(value, "", &mut out);
    out
}

fn collect_sentinels(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if v.as_str() == Some(REDACTED) {
                    out.push(p);
                } else {
                    collect_sentinels(v, &p, out);
                }
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                let p = format!("{path}[{i}]");
                if v.as_str() == Some(REDACTED) {
                    out.push(p);
                } else {
                    collect_sentinels(v, &p, out);
                }
            }
        }
        _ => {}
    }
}

/// Mask secrets in raw source text without reformatting it.
///
/// Handles `"key": "value"` (JSON) and `key = "value"` (TOML). It also masks any
/// value that structural masking would have replaced, which is what makes
/// `env` / `headers` literals safe even though a line scanner cannot tell which
/// object it is inside. Works on files that do not parse, since a broken config
/// still must not leak a credential into the browser.
pub fn mask_text(text: &str) -> String {
    let literals = serde_json::from_str::<Value>(text)
        .ok()
        .map(|value| sensitive_literals(&value))
        .unwrap_or_default();

    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&mask_line(line, &literals));
    }
    out
}

/// Length in bytes of the UTF-8 character starting at `i`.
fn char_len(bytes: &[u8], i: usize) -> usize {
    let b = bytes[i];
    let len = if b < 0x80 {
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        // Continuation byte at a char boundary: copy it alone rather than panic.
        1
    };
    len.min(bytes.len() - i).max(1)
}

/// Copy one whole character (never a partial UTF-8 sequence) and advance.
fn push_char(out: &mut String, line: &str, i: usize, bytes: &[u8]) -> usize {
    let next = i + char_len(bytes, i);
    out.push_str(&line[i..next]);
    next
}

fn mask_line(line: &str, literals: &BTreeSet<String>) -> String {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut out = String::with_capacity(line.len());

    while i < bytes.len() {
        let start = i;
        // A key is either quoted (JSON) or a bare identifier (TOML).
        let parsed = if bytes[i] == b'"' || bytes[i] == b'\'' {
            read_quoted(line, i, bytes[i])
        } else if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            read_bare_key(line, i)
        } else {
            None
        };

        let Some((key, after_key)) = parsed else {
            i = push_char(&mut out, line, i, bytes);
            continue;
        };

        // Expect `:` or `=`, then a quoted value.
        let mut j = after_key;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || (bytes[j] != b':' && bytes[j] != b'=') {
            // Not a key: emit the token verbatim and resume after it, so that a
            // string literal's contents are never rescanned as if they were code.
            out.push_str(&line[start..after_key]);
            i = after_key;
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || (bytes[j] != b'"' && bytes[j] != b'\'') {
            out.push_str(&line[start..after_key]);
            i = after_key;
            continue;
        }

        let quote = bytes[j];
        let Some((raw_value, value_end)) = read_quoted(line, j, quote) else {
            out.push_str(&line[start..after_key]);
            i = after_key;
            continue;
        };

        out.push_str(&line[start..j]);
        let replacement = !is_env_reference(&raw_value)
            && (is_sensitive_key(&key) || literals.contains(&raw_value));
        if replacement {
            // Keep the original quoting so the masked text has the same shape
            // (and, for JSON, still parses) instead of becoming invalid syntax.
            out.push(quote as char);
            out.push_str(REDACTED);
            out.push(quote as char);
        } else {
            out.push_str(&line[j..value_end]);
        }
        i = value_end;
    }

    out
}

/// Read a bare identifier key (TOML): `[A-Za-z_][A-Za-z0-9_.-]*`.
fn read_bare_key(line: &str, start: usize) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let mut i = start;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
    {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((line[start..i].to_string(), i))
}

/// Read a quoted string starting at `start`; returns the unescaped content and
/// the index just past the closing quote.
fn read_quoted(line: &str, start: usize, quote: u8) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let mut i = start + 1;
    let mut content = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 < bytes.len() {
                    content.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    return None;
                }
            }
            b if b == quote => return Some((content, i + 1)),
            b => {
                // Preserve multi-byte characters instead of byte-widening them.
                let len = char_len(bytes, i);
                content.push_str(&line[i..i + len]);
                i += len;
                let _ = b;
            }
        }
    }
    None
}

/// Whether a text contains any masked field (cheap pre-check for the UI).
pub fn text_contains_sensitive_key(text: &str) -> bool {
    let mut found = BTreeSet::new();
    for line in text.lines() {
        for token in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
            if token.is_empty() {
                continue;
            }
            if is_sensitive_key(token) {
                found.insert(token.to_string());
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sensitive_key_detection_matches_real_shapes() {
        for key in [
            "apiKey",
            "api_key",
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "accessToken",
            "Authorization",
            "x-api-key",
            "clientSecret",
            "password",
        ] {
            assert!(is_sensitive_key(key), "{key} should be sensitive");
        }
    }

    #[test]
    fn benign_keys_are_not_masked() {
        for key in [
            "maxTokens",
            "maxOutputBytes",
            "contextWindow",
            "thinkingLevelMap",
            "disabledServers",
            "toolExposure",
            "tokenBudget",
            "keymap",
        ] {
            assert!(!is_sensitive_key(key), "{key} should not be sensitive");
        }
    }

    #[test]
    fn env_references_are_shown_verbatim() {
        assert!(is_env_reference("${OPENAI_API_KEY}"));
        assert!(is_env_reference("$OPENAI_API_KEY"));
        assert!(!is_env_reference("sk-live-abcdef0123456789"));
    }

    #[test]
    fn mask_value_hides_literal_and_keeps_reference() {
        let value = json!({
            "providers": {
                "openai": { "apiKey": "sk-live-abcdef0123456789", "baseUrl": "https://x" },
                "zen": { "apiKey": "${ZEN_KEY}" }
            }
        });
        let (masked, paths) = mask_value(&value);
        assert_eq!(masked["providers"]["openai"]["apiKey"], REDACTED);
        assert_eq!(masked["providers"]["zen"]["apiKey"], "${ZEN_KEY}");
        assert_eq!(masked["providers"]["openai"]["baseUrl"], "https://x");
        assert_eq!(paths, vec!["providers.openai.apiKey"]);
    }

    #[test]
    fn mask_value_applies_token_heuristic_inside_env() {
        let value = json!({
            "mcpServers": {
                "srv": {
                    "env": {
                        "PATH": "/usr/local/bin:/usr/bin:/bin",
                        "NODE_ENV": "production",
                        "FOO_SECRET_VALUE": "a1b2c3d4e5f6g7h8i9j0k1l2",
                        "OPENAI_KEY": "${OPENAI_API_KEY}"
                    }
                }
            }
        });
        let (masked, _) = mask_value(&value);
        let env = &masked["mcpServers"]["srv"]["env"];
        assert_eq!(env["PATH"], "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(env["NODE_ENV"], "production");
        assert_eq!(env["OPENAI_KEY"], "${OPENAI_API_KEY}");
        assert_eq!(env["FOO_SECRET_VALUE"], REDACTED);
    }

    #[test]
    fn restore_keeps_replaces_and_clears() {
        let existing = json!({
            "apiKey": "sk-real-key",
            "token": "tok-real",
            "other": "keep-me"
        });
        // 保持 apiKey, 替换 token, 清除 other.
        let mut draft = json!({ "apiKey": REDACTED, "token": "tok-new" });
        restore_secrets(&mut draft, &existing);
        assert_eq!(draft["apiKey"], "sk-real-key");
        assert_eq!(draft["token"], "tok-new");
        assert!(draft.get("other").is_none());
    }

    #[test]
    fn restore_leaves_unrestorable_sentinel_for_caller() {
        let existing = json!({});
        let mut draft = json!({ "apiKey": REDACTED });
        restore_secrets(&mut draft, &existing);
        assert_eq!(draft["apiKey"], REDACTED);
        assert_eq!(dangling_sentinels(&draft), vec!["apiKey"]);
    }

    #[test]
    fn restore_detects_dangling_sentinels() {
        let draft = json!({ "nested": { "deep": [REDACTED] } });
        assert_eq!(dangling_sentinels(&draft), vec!["nested.deep[0]"]);
    }

    #[test]
    fn mask_text_preserves_layout_and_masks_json_and_toml() {
        let json_text = "{\n  \"apiKey\": \"sk-real\",\n  \"model\": \"gpt-4o\"\n}\n";
        let masked = mask_text(json_text);
        assert!(masked.contains("\"apiKey\": \"***REDACTED***\""));
        assert!(masked.contains("\"model\": \"gpt-4o\""));
        assert_eq!(masked.lines().count(), json_text.lines().count());

        let toml_text =
            "[providers.xai]\napi_key = \"xai-real\"\nbase_url = \"https://api.x.ai\"\n";
        let masked = mask_text(toml_text);
        assert!(masked.contains("api_key = \"***REDACTED***\""));
        assert!(masked.contains("base_url = \"https://api.x.ai\""));
    }

    #[test]
    fn mask_text_is_idempotent_and_indentation_safe() {
        let text = "  \"apiKey\": \"secret-value\",\n";
        let once = mask_text(text);
        let twice = mask_text(&once);
        assert_eq!(once, twice);
        assert!(once.starts_with("  \"apiKey\": "));
    }

    #[test]
    fn mask_text_marks_headers() {
        let text = "{\"headers\": {\"Authorization\": \"Bearer abc123\"}}";
        let masked = mask_text(text);
        assert!(masked.contains("\"Authorization\": \"***REDACTED***\""));
    }
}
