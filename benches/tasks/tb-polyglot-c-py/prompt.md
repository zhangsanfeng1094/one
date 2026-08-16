# Polyglot C/Python (Terminal-Bench 2.0)

Write a **single file** `polyglot/main.py.c` that is a polyglot: it must run correctly as both Python 3 and C.

```bash
python3 polyglot/main.py.c N
# and
gcc polyglot/main.py.c -o polyglot/cmain && ./polyglot/cmain N
```

Both must print the **k-th Fibonacci number** to stdout where:

- `f(0) = 0`, `f(1) = 1`, `f(2) = 1`, `f(3) = 2`, …

Only `main.py.c` may exist under `polyglot/` (no other helper source files).

Scoring: `python3 verify.py`.
