//! Smart quotes must remain U+201C/U+201D.

pub fn motto() -> &'static str {
    "“gauntlet”"
}

pub fn score_line(v: i32) -> String {
    // BUG: should multiply by 10, currently adds 10
    format!("val={}", v + 10)
}
