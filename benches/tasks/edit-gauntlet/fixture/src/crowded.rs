//! Many near-identical `n + 1` expressions. Only the marked target must change.

pub fn pipeline(n: i32) -> i32 {
    let mut n = n;
    // step 1
    n = n + 1;
    // step 2
    n = n + 1;
    // step 3
    n = n + 1;
    // step 4
    n = n + 1;
    // step 5
    n = n + 1;
    // step 6
    n = n + 1;
    // step 7
    n = n + 1;
    // step 8
    n = n + 1;
    // TARGET_SITE_9: must become n * 3 (not n + 1)
    n = n + 1;
    // step 10
    n = n + 1;
    // step 11
    n = n + 1;
    // step 12
    n = n + 1;
    // step 13
    n = n + 1;
    // step 14
    n = n + 1;
    // step 15
    n = n + 1;
    n
}
