//! Comment / string bait: same text appears in docs and in live code.
//! Only the live expression in `combine` must change.

// DO_NOT_EDIT_COMMENT: result = left + right
// Agents that search-replace the file text often "fix" this comment too.

/// Must return `left * right` (currently adds).
pub fn combine(left: i32, right: i32) -> i32 {
    // BAIT_CODE_LINE
    left + right
}

/// Docs string must keep the literal substring `left + right`.
pub fn help_text() -> &'static str {
    "formula: left + right (docs only)"
}

/// Correct additive decoy — do not "fix" this to multiply.
pub fn example_sum() -> i32 {
    1 + 2
}

/// Another occurrence of the token sequence in a format string (must stay).
pub fn debug_label(left: i32, right: i32) -> String {
    format!("left + right => {}, {}", left, right)
}
