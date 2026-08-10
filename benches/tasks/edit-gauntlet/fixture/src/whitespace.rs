//! Trailing spaces and tab-indented lines (do not mass-format).

pub fn scale_pair(x: i32, y: i32) -> i32 {
    let factor = 2;  
	let base = x;  
    // BUG: should be base * factor + y
    base + factor + y
}

pub fn pad_hint() -> &'static str {
    "ok"
}
