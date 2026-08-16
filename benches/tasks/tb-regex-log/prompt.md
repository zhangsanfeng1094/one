# Regex Log Dates (Terminal-Bench 2.0)

Write a regex expression that matches dates in the format `YYYY-MM-DD` appearing in lines that contain an IPv4 address in a log file.

Rules:

- If multiple dates are present in a line, match only the **last** date in that line.
- February can have up to 29 days in all years (no leap-year distinction).
- IPv4 uses normal decimal notation without leading zeros in each octet.
- Avoid false matches: valid dates and IPv4 addresses must **not** be immediately preceded or followed by alphanumeric characters (e.g. `user 1134-12-1234` is not a date).

Save your regex in **`regex.txt`** (workspace root).

The harness reads the pattern and applies it with:

```python
import re
matches = re.findall(pattern, log_text, re.MULTILINE)
```

Scoring: `python3 verify.py`.
