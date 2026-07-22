/// Build a stable public tool name: `{server}__{tool}`.
///
/// Names are sanitized so they only contain `[A-Za-z0-9_-]`.
pub fn public_tool_name(server: &str, tool: &str) -> String {
    format!("{}__{}", sanitize(server), sanitize(tool))
}

pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_name_basic() {
        assert_eq!(
            public_tool_name("github", "create_issue"),
            "github__create_issue"
        );
    }

    #[test]
    fn sanitize_special() {
        assert_eq!(
            public_tool_name("my server", "list/prs"),
            "my_server__list_prs"
        );
    }
}
