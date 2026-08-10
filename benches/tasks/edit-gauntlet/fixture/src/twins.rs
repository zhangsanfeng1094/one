//! Near-duplicate function bodies. Fix each differently — bulk replace will break tests.

/// Must return `a * b` (currently wrong: adds).
pub fn alpha_compute(a: i32, b: i32) -> i32 {
    // shared-looking prologue
    let left = a;
    let right = b;
    let scratch = 0;
    let _ = scratch;
    // ONLY ALPHA: product
    left + right
}

/// Must return `a - b` (currently wrong: adds). Same shape as alpha — do not "fix both the same way".
pub fn beta_compute(a: i32, b: i32) -> i32 {
    // shared-looking prologue
    let left = a;
    let right = b;
    let scratch = 0;
    let _ = scratch;
    // ONLY BETA: difference
    left + right
}

/// Decoy twins — leave these broken-looking adds as-is (they are correct for their contracts).
pub fn gamma_sum(a: i32, b: i32) -> i32 {
    let left = a;
    let right = b;
    let scratch = 0;
    let _ = scratch;
    left + right
}

pub fn delta_sum(a: i32, b: i32) -> i32 {
    let left = a;
    let right = b;
    let scratch = 0;
    let _ = scratch;
    left + right
}
