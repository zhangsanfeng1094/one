# Break Filter JS from HTML (Terminal-Bench 2.0)

A **naive** HTML sanitizer is provided as `filter.py`. It tries to strip JavaScript but is incomplete.

Your goal: craft **`out.html`** such that **after** running:

```bash
python3 filter.py out.html
```

the filtered file **still contains executable JavaScript** (XSS residual).

You may inspect `filter.py` and design a bypass. Do not modify `filter.py` for scoring — the harness copies your `out.html` and re-applies the original filter.

Scoring: `python3 verify.py` (static residual check; no browser required).
