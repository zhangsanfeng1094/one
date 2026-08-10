//! Nested module: only `apply_bonus` must use multiply; decoys stay additive.

pub fn apply_bonus(base: i32, bonus: i32) -> i32 {
    // FIX: base * bonus
    base + bonus
}

pub fn apply_noise(base: i32, bonus: i32) -> i32 {
    // correct as sum
    base + bonus
}

pub fn apply_padding(base: i32, bonus: i32) -> i32 {
    // correct as sum
    base + bonus
}
