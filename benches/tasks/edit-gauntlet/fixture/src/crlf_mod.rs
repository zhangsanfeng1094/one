//! Entire file is CRLF. Do not rewrite whole file to LF.

pub fn triple(n: i32) -> i32 {
    let k = 3;  
    n + k // BUG: must be n * k
}
