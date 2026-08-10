//! Same identifier / expression text in nested scopes. Edit the wrong one → fail.

pub fn outer_adjust(n: i32) -> i32 {
    let value = n + 1; // OUTER_KEEP: stay n + 1
    let inner = {
        let value = n + 1; // INNER_FIX: must become n * 3
        value
    };
    value + inner
}

pub fn outer_only(n: i32) -> i32 {
    let value = n + 1;
    value
}
