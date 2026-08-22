//! Query token matching for the intent graph.
//!
//! Verb-object matching used to be raw `str::contains`, which made single-char
//! CJK tokens like 查/库 fire inside 检查/仓库. Matching here uses:
//! - ASCII word boundaries
//! - CJK blocked-compound lists for 1-char tokens
//! - Verb families so 看/查/search share recall without bloating learned rules

/// Strip XML tags (e.g. `<system-reminder>...</system-reminder>`) and meta formatting.
pub fn strip_xml_and_meta(text: &str) -> String {
    let mut cleaned = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            cleaned.push(ch);
        }
    }
    cleaned
}

/// FNV-1a 64-bit — stable across Rust versions and platforms (unlike DefaultHasher).
pub fn stable_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Check if a query is purely seeking conceptual clarification or confirmation.
///
/// Bare 「是不是」 / 「是一种」 are *not* enough — they show up in real task
/// statements ("这个是不是 bug").
pub fn is_conceptual_clarification(query: &str) -> bool {
    let q_lower = query.to_ascii_lowercase();
    let clarification_markers = [
        "可以理解为",
        "我可以理解",
        "可以理解",
        "是不是说",
        "你的意思是",
        "你的意思",
        "也就是说",
        "如何理解",
        "怎么理解",
        "区别是什么",
        "如何评价",
        "为什么说",
        "算不算是",
        "意思是不是",
        "是这个意思",
    ];
    clarification_markers.iter().any(|m| q_lower.contains(m))
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
    )
}

fn is_ascii_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Compounds where a 1-char CJK token must *not* fire.
fn blocked_compounds(token: &str) -> &'static [&'static str] {
    match token {
        "查" => &[
            "检查", "调查", "审查", "排查", "清查", "普查", "勘查", "稽查", "探查", "查杀", "查封",
            "查明",
        ],
        "看" => &[
            "看守", "难看", "看待", "看齐", "看管", "看中", "看上", "看门", "看护", "看来", "看跌",
            "看涨",
        ],
        "库" => &[
            "仓库",
            "数据库",
            "知识库",
            "语料库",
            "字库",
            "图库",
            "题库",
            "金库",
            "水库",
            "车库",
            "入库",
            "出库",
            "库存",
        ],
        "跑" => &["逃跑", "奔跑", "跑道", "跑车", "跑路", "空跑", "跑题"],
        "找" => &["自找"],
        _ => &[],
    }
}

const FAMILY_INVESTIGATE: &[&str] = &[
    "查",
    "找",
    "搜",
    "检索",
    "查阅",
    "看",
    "看看",
    "分析",
    "调研",
    "了解",
    "怎么用",
    "如何用",
    "search",
    "lookup",
    "find",
    "locate",
    "定位",
];
const FAMILY_RUN: &[&str] = &["跑", "运行", "执行", "run"];
const FAMILY_TEST: &[&str] = &["测试", "单测", "test"];
const FAMILY_GIT_FORCE: &[&str] = &[
    "强制推送",
    "force push",
    "force-push",
    "push --force",
    "push -f",
];

fn verb_family(token: &str) -> &'static [&'static str] {
    let t = token.trim();
    if FAMILY_INVESTIGATE.iter().any(|v| *v == t) {
        FAMILY_INVESTIGATE
    } else if FAMILY_RUN.iter().any(|v| *v == t) {
        FAMILY_RUN
    } else if FAMILY_TEST.iter().any(|v| *v == t) {
        FAMILY_TEST
    } else if FAMILY_GIT_FORCE.iter().any(|v| *v == t) {
        FAMILY_GIT_FORCE
    } else {
        &[]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TokenRole {
    Verb,
    Object,
}

fn span_blocked(haystack: &str, start: usize, end: usize, token: &str) -> bool {
    for compound in blocked_compounds(token) {
        for (i, m) in haystack.match_indices(*compound) {
            if i <= start && i + m.len() >= end {
                return true;
            }
        }
    }
    false
}

fn ascii_word_bounded(haystack: &str, start: usize, end: usize) -> bool {
    let before_ok = haystack
        .get(..start)
        .and_then(|s| s.chars().last())
        .map(|c| !is_ascii_word_char(c))
        .unwrap_or(true);
    let after_ok = haystack
        .get(end..)
        .and_then(|s| s.chars().next())
        .map(|c| !is_ascii_word_char(c))
        .unwrap_or(true);
    before_ok && after_ok
}

/// True when `token` occurs in `query` under the matching rules for `role`.
pub fn token_in_text(query: &str, token: &str, role: TokenRole) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }

    if role == TokenRole::Verb {
        let family = verb_family(token);
        if !family.is_empty() {
            return family.iter().any(|alias| phrase_in_text(query, alias));
        }
    }

    phrase_in_text(query, token)
}

/// Match a single phrase (no verb-family expansion).
pub fn phrase_in_text(query: &str, phrase: &str) -> bool {
    let phrase = phrase.trim();
    if phrase.is_empty() {
        return false;
    }

    let q = query.to_ascii_lowercase();
    let p = phrase.to_ascii_lowercase();

    let ascii_only = p.chars().all(|c| c.is_ascii());
    let cjk_len = p.chars().filter(|c| is_cjk(*c)).count();
    let char_len = p.chars().count();

    for (start, matched) in q.match_indices(&p) {
        let end = start + matched.len();
        if ascii_only && !p.contains(' ') && p.chars().all(is_ascii_word_char) {
            if ascii_word_bounded(&q, start, end) {
                return true;
            }
            continue;
        }
        if cjk_len == 1 && char_len == 1 {
            if !span_blocked(&q, start, end, phrase) {
                return true;
            }
            continue;
        }
        return true;
    }
    false
}

const LATIN_IDENT_STOP: &[&str] = &[
    "the", "this", "that", "with", "from", "http", "https", "and", "for", "you", "your", "git",
    "src", "lib", "bin", "app", "cli", "cmd", "json", "yaml", "toml", "true", "false", "null",
    "none", "todo", "system", "tools", "tool", "user", "please", "help", "just", "into", "over",
    "under", "about", "when", "what", "how", "why", "run", "test", "push", "reset", "hard",
    "force", "docs", "api", "sdk", "package", "crate", "find", "search", "lookup", "build",
    "debug", "deploy", "review", "check", "code", "file", "path", "name", "type", "list", "main",
    "mod", "use", "let", "pub", "new", "err", "ok", "std",
];

/// Crate / library-like latin identifiers (tokio, reqwest, hermes, …).
pub fn extract_latin_idents(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 3 {
            let token = cur.to_ascii_lowercase();
            let has_letter = token.chars().any(|c| c.is_ascii_alphabetic());
            if has_letter
                && !token.starts_with('-')
                && !token.ends_with('-')
                && !LATIN_IDENT_STOP.contains(&token.as_str())
                && !out.contains(&token)
            {
                out.push(token);
            }
        }
        cur.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if ch == '-' && !cur.is_empty() {
            cur.push(ch);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

pub fn has_crate_like_ident(text: &str) -> bool {
    !extract_latin_idents(text).is_empty()
}

const COMMON_VERBS: &[&str] = &[
    "查阅", "检索", "分析", "调研", "了解", "看看", "运行", "执行", "删除", "重置", "修改", "重构",
    "编写", "测试", "部署", "检查", "定位", "实现", "查询", "查找", "搜索", "push", "reset",
    "check", "find", "search", "test", "build", "debug", "deploy", "review", "run", "查", "找",
    "搜", "看", "跑",
];

const COMMON_DOMAIN_OBJECTS: &[&str] = &[
    "设计原理",
    "使用方式",
    "使用方法",
    "架构",
    "源码",
    "代码",
    "设计",
    "文档",
    "资料",
    "说明",
    "用法",
    "接口",
    "配置",
    "依赖",
    "数据库",
    "测试",
    "单测",
    "部署",
    "环境",
    "迁移",
    "分支",
    "仓库",
    "上下文",
    "记忆",
    "缓存",
    "网络",
    "并发",
    "模型",
    "工具",
    "函数",
    "模块",
    "规范",
    "安全",
    "权限",
    "重构",
    "路由",
    "crate",
    "package",
    "sdk",
    "api",
    "docs",
];

const STOP_WORDS: &[&str] = &[
    "当",
    "如果",
    "遇到",
    "涉及",
    "若",
    "针对",
    "用户",
    "询问",
    "要求",
    "输入",
    "说",
    "想",
    "要",
    "的",
    "或",
    "与",
    "和",
    "在",
    "时",
    "了",
    "等",
    "进行",
    "一个",
    "一些",
    "所有",
    "这种",
    "那种",
    "如何",
    "怎么",
    "方式",
    "方法",
    "帮我",
    "请",
    "一下",
    "这个",
    "那个",
    "看看",
    "是",
    "不是",
    "吧",
    "啊",
    "吗",
    "呢",
    "理解",
    "解释",
    "认为",
    "觉得",
    "所谓",
    "可以",
    "能不能",
    "是不是",
    "意思是",
    "就是",
    "装到",
    "里面",
    "外面",
    "探讨",
    "确认",
];

/// Extract (verbs, objects) for learning — no default verb, no synonym flooding.
pub fn extract_verbs_and_objects(text: &str) -> (Vec<String>, Vec<String>) {
    let clean_text = strip_xml_and_meta(text);
    let mut verbs: Vec<String> = Vec::new();
    let mut objects: Vec<String> = Vec::new();

    for &v in COMMON_VERBS {
        if token_in_text(&clean_text, v, TokenRole::Verb) && !verbs.iter().any(|x| x == v) {
            // Record the surface token, not the whole family.
            if phrase_in_text(&clean_text, v) {
                verbs.push(v.to_string());
            }
        }
    }

    for &obj in COMMON_DOMAIN_OBJECTS {
        if phrase_in_text(&clean_text, obj) && !objects.iter().any(|o| o == obj) {
            objects.push(obj.to_string());
        }
    }

    for ident in extract_latin_idents(&clean_text) {
        if !verbs.contains(&ident) && !objects.contains(&ident) {
            objects.push(ident);
        }
    }

    for token in clean_text.split([
        ' ', '、', '，', ',', '。', '/', '+', '与', '或', '：', ':', '\n', '\t', '"', '\'',
    ]) {
        let token = token.trim();
        if token.chars().count() >= 2
            && !STOP_WORDS.contains(&token)
            && !verbs.iter().any(|v| v == token)
            && !objects.iter().any(|o| o == token)
            && token.chars().any(is_cjk)
        {
            objects.push(token.to_string());
        }
    }

    (verbs, objects)
}

/// Prefer latin idents and short domain terms; drop noisy leftovers.
pub fn cap_learned_objects(objects: Vec<String>, limit: usize) -> Vec<String> {
    let mut idents = Vec::new();
    let mut rest = Vec::new();
    for o in objects {
        if o.chars().all(|c| is_ascii_word_char(c)) {
            idents.push(o);
        } else {
            rest.push(o);
        }
    }
    rest.sort_by_key(|s| s.chars().count());
    let mut out = idents;
    out.extend(rest);
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_single_char_does_not_fire_inside_compounds() {
        assert!(!phrase_in_text("检查一下这个仓库", "查"));
        assert!(!phrase_in_text("检查一下这个仓库", "库"));
        assert!(!phrase_in_text("检查一下这个数据库配置", "库"));
        assert!(phrase_in_text("帮我查一下资料", "查"));
        assert!(phrase_in_text("看看这个库怎么用", "库"));
        assert!(phrase_in_text("查阅开源库文档", "库"));
    }

    #[test]
    fn ascii_uses_word_boundary() {
        assert!(phrase_in_text("search for serde docs", "search"));
        assert!(phrase_in_text("search for serde docs", "serde"));
        assert!(!phrase_in_text("contest results", "test"));
        assert!(phrase_in_text("run cargo test", "test"));
        assert!(phrase_in_text("git push --force", "--force"));
    }

    #[test]
    fn investigate_family_shares_recall() {
        assert!(token_in_text("分析 hermes 的源码", "看", TokenRole::Verb));
        assert!(token_in_text("帮我查一下 tokio", "search", TokenRole::Verb));
    }

    #[test]
    fn clarification_requires_strong_markers() {
        assert!(is_conceptual_clarification("我可以理解为这是一种约束吗"));
        assert!(!is_conceptual_clarification("这个是不是 bug"));
        assert!(!is_conceptual_clarification(
            "Rust 是一种系统语言，帮我改编译参数"
        ));
    }

    #[test]
    fn extract_skips_default_verb() {
        let (verbs, objects) = extract_verbs_and_objects("hermes 上下文");
        assert!(verbs.is_empty());
        assert!(objects.iter().any(|o| o == "hermes"));
        assert!(objects.iter().any(|o| o == "上下文"));
    }
}
