//! Agent Skills ([agentskills.io](https://agentskills.io)) discovery & disclosure.
//!
//! Progressive disclosure (same as Pi):
//! 1. **Catalog** — only `name` + `description` (+ location) in the system prompt
//! 2. **Instructions** — model loads full `SKILL.md` via the `read` tool when relevant
//! 3. **Resources** — scripts/references loaded on demand from the skill directory
//!
//! `/skill:name` is optional **user-explicit** activation (force-load), not the primary path.

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Skill {
    /// From frontmatter `name` (or directory name fallback).
    pub name: String,
    /// From frontmatter `description` (required to appear in catalog).
    pub description: String,
    /// Absolute path to `SKILL.md`.
    pub location: PathBuf,
    /// Full file content (frontmatter + body). Cached for `/skill:name` force-load.
    pub content: String,
    /// Markdown body after frontmatter.
    pub body: String,
    /// When true, omit from model catalog; only `/skill:name` can activate.
    pub disable_model_invocation: bool,
}

impl Skill {
    /// Directory containing `SKILL.md` (base for relative scripts/references).
    pub fn base_dir(&self) -> &Path {
        self.location
            .parent()
            .unwrap_or_else(|| Path::new("."))
    }
}

/// Discover skills under the given roots (non-recursive root listing of skill dirs,
/// recursive search for `SKILL.md` under each).
pub async fn discover_skills(dirs: &[PathBuf]) -> Result<Vec<Skill>> {
    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for root in dirs {
        if !root.exists() {
            continue;
        }
        collect_skills_under(root, &mut skills, &mut seen_names, 0).await?;
    }

    Ok(skills)
}

const MAX_DEPTH: u32 = 5;

async fn collect_skills_under(
    dir: &Path,
    skills: &mut Vec<Skill>,
    seen: &mut std::collections::HashSet<String>,
    depth: u32,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        if let Some(skill) = load_skill_file(&skill_file).await? {
            // First-found wins (caller should pass roots in precedence order).
            if seen.insert(skill.name.clone()) {
                skills.push(skill);
            }
        }
        // Directory with SKILL.md is a skill package — don't recurse into scripts/refs as skills.
        return Ok(());
    }

    let mut entries = match fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        Box::pin(collect_skills_under(&path, skills, seen, depth + 1)).await?;
    }
    Ok(())
}

async fn load_skill_file(path: &Path) -> Result<Option<Skill>> {
    let content = fs::read_to_string(path).await?;
    let parsed = parse_skill_md(&content, path);
    Ok(parsed)
}

#[derive(Debug, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    disable_model_invocation: bool,
}

/// Parse YAML-ish frontmatter + body. Lenient (Pi-style): missing description → skip skill.
pub fn parse_skill_md(content: &str, path: &Path) -> Option<Skill> {
    let (fm, body) = split_frontmatter(content);
    let meta = parse_frontmatter_fields(&fm);

    let description = meta
        .description
        .filter(|d| !d.trim().is_empty())?;

    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("skill")
        .to_string();

    let name = meta
        .name
        .filter(|n| !n.is_empty())
        .unwrap_or(dir_name);

    let location = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());

    Some(Skill {
        name,
        description: description.trim().to_string(),
        location,
        content: content.to_string(),
        body: body.trim().to_string(),
        disable_model_invocation: meta.disable_model_invocation,
    })
}

fn split_frontmatter(content: &str) -> (String, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (String::new(), content.to_string());
    }
    let after = &trimmed[3..];
    // Skip optional newline after opening ---
    let after = after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n")).unwrap_or(after);
    // Find closing ---
    if let Some(end) = after.find("\n---") {
        let fm = after[..end].to_string();
        let rest = &after[end + 4..]; // skip \n---
        let body = rest
            .strip_prefix('\n')
            .or_else(|| rest.strip_prefix("\r\n"))
            .unwrap_or(rest)
            .to_string();
        return (fm, body);
    }
    (String::new(), content.to_string())
}

fn parse_frontmatter_fields(fm: &str) -> Frontmatter {
    let mut out = Frontmatter::default();
    let lines: Vec<&str> = fm.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        // YAML folded (`>`) / literal (`|`) block scalars, or plain multi-line
        // indented continuations (common in Agent Skills descriptions).
        let (scalar, consumed) = parse_yaml_scalar(value, &lines[i..]);
        i += consumed;

        match key {
            "name" => out.name = Some(unquote(&scalar)),
            "description" => {
                let d = unquote(&scalar);
                if !d.is_empty() {
                    out.description = Some(d);
                }
            }
            "disable-model-invocation" => {
                out.disable_model_invocation =
                    scalar.eq_ignore_ascii_case("true") || scalar == "1" || scalar == "yes";
            }
            _ => {}
        }
    }
    out
}

/// Parse a YAML-ish scalar: single-line, `>` / `|` block, or empty value + indented lines.
/// Returns `(value, extra_lines_consumed)`.
fn parse_yaml_scalar(value: &str, rest: &[&str]) -> (String, usize) {
    let value = value.trim();
    if value == ">" || value == "|" || value == ">-" || value == "|-" || value == ">+" || value == "|+"
    {
        let folded = value.starts_with('>');
        let (body, n) = take_indented_block(rest);
        let text = if folded {
            collapse_folded(&body)
        } else {
            body.join("\n")
        };
        return (text, n);
    }
    if value.is_empty() {
        let (body, n) = take_indented_block(rest);
        if n > 0 {
            return (collapse_folded(&body), n);
        }
        return (String::new(), 0);
    }
    (unquote(value), 0)
}

fn take_indented_block(rest: &[&str]) -> (Vec<String>, usize) {
    let mut body = Vec::new();
    let mut n = 0;
    for line in rest {
        // Blank lines inside a block are kept as empty paragraphs for `|`;
        // for simplicity we keep them and let fold collapse later.
        if line.is_empty() {
            if body.is_empty() {
                n += 1;
                continue;
            }
            body.push(String::new());
            n += 1;
            continue;
        }
        // Indented continuation (at least one space or tab).
        if line.starts_with(' ') || line.starts_with('\t') {
            body.push(line.trim().to_string());
            n += 1;
            continue;
        }
        break;
    }
    // Trim trailing empty lines collected after the block content.
    while body.last().is_some_and(|s| s.is_empty()) {
        body.pop();
    }
    (body, n)
}

fn collapse_folded(lines: &[String]) -> String {
    let mut parts = Vec::new();
    let mut para = Vec::new();
    for line in lines {
        if line.is_empty() {
            if !para.is_empty() {
                parts.push(para.join(" "));
                para.clear();
            }
        } else {
            para.push(line.as_str());
        }
    }
    if !para.is_empty() {
        parts.push(para.join(" "));
    }
    parts.join("\n")
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Tier-1 catalog for the system prompt (XML per agentskills.io integrate guide).
/// Excludes `disable_model_invocation` skills.
pub fn skills_catalog_xml(skills: &[Skill]) -> Option<String> {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return None;
    }

    let mut out = String::from(
        "The following skills provide specialized instructions for specific tasks.\n\
         When a task matches a skill's description, use the `read` tool to load the\n\
         SKILL.md at the listed <location> **before** proceeding.\n\
         Relative paths inside a skill (scripts/, references/, assets/) are relative\n\
         to that skill's directory (parent of SKILL.md); prefer absolute paths in tool calls.\n\
         Do not invent skill content — always read the file.\n\n\
         <available_skills>\n",
    );
    for skill in visible {
        let loc = skill.location.display();
        // Escape minimal XML specials in description
        let desc = xml_escape(&skill.description);
        let name = xml_escape(&skill.name);
        out.push_str(&format!(
            "  <skill>\n    <name>{name}</name>\n    <description>{desc}</description>\n    <location>{loc}</location>\n  </skill>\n"
        ));
    }
    out.push_str("</available_skills>");
    Some(out)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Resolve user-explicit `/skill:name` or `/skill name` (optional force-load).
pub fn resolve_skill_invocation<'a>(
    skills: &'a [Skill],
    input: &str,
) -> Option<(&'a Skill, String)> {
    let trimmed = input.trim();
    let rest = if let Some(r) = trimmed.strip_prefix("/skill:") {
        r
    } else if let Some(r) = trimmed.strip_prefix("/skill ") {
        r
    } else if let Some(r) = trimmed.strip_prefix("/skill\t") {
        r
    } else {
        return None;
    };

    let rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }

    let (name, extra) = match rest.split_once(|c: char| c.is_whitespace()) {
        Some((n, e)) => (n, e.trim().to_string()),
        None => (rest, String::new()),
    };

    let skill = skills
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))?;
    Some((skill, extra))
}

/// Payload injected when the user force-loads a skill (Pi: body + `User: args`).
/// Expands `{baseDir}` placeholders like Pi / pi-skills.
pub fn force_load_message(skill: &Skill, user_args: &str) -> String {
    let dir = skill.base_dir().display().to_string();
    let body = if skill.body.is_empty() {
        skill.content.as_str()
    } else {
        skill.body.as_str()
    };
    let body = body
        .replace("{baseDir}", &dir)
        .replace("{BASE_DIR}", &dir);
    let mut msg = format!(
        "<skill_content name=\"{}\">\n\
         Skill directory: {dir}\n\
         Relative paths in this skill are relative to the skill directory.\n\n\
         {body}\n\
         </skill_content>",
        skill.name
    );
    if !user_args.is_empty() {
        msg.push_str("\n\nUser: ");
        msg.push_str(user_args);
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_frontmatter() {
        let md = r#"---
name: pdf-processing
description: Extract PDF text. Use when handling PDFs.
disable-model-invocation: false
---

# PDF
Run scripts/extract.py
"#;
        let skill = parse_skill_md(md, Path::new("/tmp/pdf-processing/SKILL.md")).unwrap();
        assert_eq!(skill.name, "pdf-processing");
        assert!(skill.description.contains("PDF"));
        assert!(skill.body.contains("extract.py"));
        assert!(!skill.disable_model_invocation);
    }

    #[test]
    fn parses_folded_description_block() {
        let md = r#"---
name: create-skill
description: >
  Interactively create a new skill.
  Use when scaffolding SKILL.md.
---

# Body
"#;
        let skill = parse_skill_md(md, Path::new("/tmp/create-skill/SKILL.md")).unwrap();
        assert_eq!(skill.name, "create-skill");
        assert!(skill.description.contains("Interactively create"));
        assert!(skill.description.contains("SKILL.md"));
        assert!(!skill.description.contains('>'));
    }

    #[test]
    fn skips_without_description() {
        let md = "---\nname: x\n---\nbody\n";
        assert!(parse_skill_md(md, Path::new("/tmp/x/SKILL.md")).is_none());
    }

    #[test]
    fn catalog_is_xml_not_full_body() {
        let skill = parse_skill_md(
            "---\nname: review\ndescription: Code review workflow.\n---\n# Secret body\n",
            Path::new("/home/u/.agents/skills/review/SKILL.md"),
        )
        .unwrap();
        let cat = skills_catalog_xml(&[skill]).unwrap();
        assert!(cat.contains("<available_skills>"));
        assert!(cat.contains("<name>review</name>"));
        assert!(cat.contains("Code review workflow"));
        assert!(cat.contains("use the `read` tool"));
        assert!(!cat.contains("Secret body"));
    }

    #[test]
    fn disable_model_invocation_hides_from_catalog() {
        let skill = parse_skill_md(
            "---\nname: secret\ndescription: Hidden.\ndisable-model-invocation: true\n---\nbody\n",
            Path::new("/tmp/secret/SKILL.md"),
        )
        .unwrap();
        assert!(skills_catalog_xml(&[skill]).is_none());
    }

    #[test]
    fn force_load_appends_user_args() {
        let skill = parse_skill_md(
            "---\nname: review\ndescription: d\n---\nDo a review.\n",
            Path::new("/tmp/review/SKILL.md"),
        )
        .unwrap();
        let msg = force_load_message(&skill, "focus on auth");
        assert!(msg.contains("Do a review"));
        assert!(msg.contains("User: focus on auth"));
        assert!(msg.contains("skill_content"));
    }

    #[test]
    fn resolve_skill_colon_form() {
        let skill = parse_skill_md(
            "---\nname: review\ndescription: d\n---\nbody\n",
            Path::new("/tmp/review/SKILL.md"),
        )
        .unwrap();
        let skills = [skill];
        let (s, extra) =
            resolve_skill_invocation(&skills, "/skill:review focus on auth").unwrap();
        assert_eq!(s.name, "review");
        assert_eq!(extra, "focus on auth");
    }
}
