//! Multi-operator expression: easy to "fix" the wrong operator or all of them.

/// Must compute `(a * b) - c`. Currently `a + b + c`.
///
/// Common mistakes:
/// - `a * b * c`
/// - `a + b - c`
/// - `a * b + c`
/// - only replacing the first `+` → `a * b + c` (still wrong)
pub fn mixed(a: i32, b: i32, c: i32) -> i32 {
    // EXPR_MIXED_TARGET
    a + b + c
}

/// Sibling with the same operators — leave as sum of three.
pub fn mixed_decoy(a: i32, b: i32, c: i32) -> i32 {
    // EXPR_MIXED_DECOY
    a + b + c
}

/// Chain of adds where ONLY the second `+ 10` becomes `* 10`.
pub fn scale_chain(n: i32) -> i32 {
    let x = n + 10;
    // EXPR_CHAIN_MID: must be x * 10
    let y = x + 10;
    let z = y + 10;
    z
}
