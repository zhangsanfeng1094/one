//! Near-duplicate multi-line blocks (block-anchor / short-match trap).
//! Only PRIMARY mid-step must change; SECONDARY stays additive.

/// PRIMARY: mid must be `stage * 2` (currently adds).
pub fn primary_pipeline(x: i32) -> i32 {
    let stage = x.wrapping_add(1);
    let guard = stage.wrapping_add(0);
    let _ = guard;
    // NEAR_TWIN_PRIMARY_MID
    let mid = stage + 2;
    let out = mid.wrapping_add(3);
    out
}

/// SECONDARY: same shape as primary — mid must stay `stage + 2`.
pub fn secondary_pipeline(x: i32) -> i32 {
    let stage = x.wrapping_add(1);
    let guard = stage.wrapping_add(0);
    let _ = guard;
    // NEAR_TWIN_SECONDARY_MID
    let mid = stage + 2;
    let out = mid.wrapping_add(3);
    out
}

/// Tertiary decoy with the same mid line — leave additive.
pub fn tertiary_pipeline(x: i32) -> i32 {
    let stage = x.wrapping_add(1);
    let guard = stage.wrapping_add(0);
    let _ = guard;
    // NEAR_TWIN_TERTIARY_MID
    let mid = stage + 2;
    let out = mid.wrapping_add(3);
    out
}
