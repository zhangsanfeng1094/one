# Path Tracing Reverse (Terminal-Bench 2.0 hard)

A compiled program **`mystery`** is in the workspace (gunzipped by the harness from `mystery.gz` if needed). Write a C program **`mystery.c`** that performs an identical operation.

You may observe input/output behavior, decompile the program, or anything else.

Constraints:

- Running `./mystery` should be identical in effect to:
  `gcc -O2 -o reversed mystery.c -lm && ./reversed`
- Your C program must be **&lt; 2KB when gzip-compressed** (`gzip -c mystery.c | wc -c`)
- Your C program must **not** invoke `./mystery`
- Fully independent: must work even if run in complete isolation

Scoring: `python3 verify.py` (may take several minutes for rendering).
If the default command timeout is too short: `ONE_BENCH_CMD_TIMEOUT=900`.
