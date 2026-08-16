# Maintainer solutions (NOT copied into agent workspace)

Stdlib-only filter sufficient for this harness's static verifier:

```python
import re
import sys

def remove_js(html: str) -> str:
    html = re.sub(r"<\s*script\b[^>]*>.*?<\s*/\s*script\s*>", "", html, flags=re.I | re.S)
    html = re.sub(r"<\s*script\b[^>]*/\s*>", "", html, flags=re.I)
    for tag in ("iframe", "object", "embed", "frame"):
        html = re.sub(rf"<\s*{tag}\b[^>]*>.*?<\s*/\s*{tag}\s*>", "", html, flags=re.I | re.S)
        html = re.sub(rf"<\s*{tag}\b[^>]*/?\s*>", "", html, flags=re.I)
    html = re.sub(r"""\s+on\w+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)""", "", html, flags=re.I)
    html = re.sub(r"javascript\s*:", "", html, flags=re.I)
    return html

if __name__ == "__main__":
    path = sys.argv[1]
    data = open(path, encoding="utf-8", errors="replace").read()
    open(path, "w", encoding="utf-8").write(remove_js(data))
```

Upstream oracle used BeautifulSoup; this port scores with static heuristics so stdlib is enough.
