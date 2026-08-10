//! Three near-identical lines in a row. Only the middle assignment changes.

/// `layers(2)` must be 16: top=3, mid=10 (`n * 5`), bot=3.
pub fn layers(n: i32) -> i32 {
    // keep as n + 1
    let top = n + 1;
    // SANDWICH_MID: must become n * 5 (not n + 1)
    let mid = n + 1;
    // keep as n + 1
    let bot = n + 1;
    top + mid + bot
}

/// Same `n + 1` pattern elsewhere — must stay.
pub fn edge_pad(n: i32) -> i32 {
    let a = n + 1;
    let b = n + 1;
    a + b
}
