#!/usr/bin/env python3
"""Naive XSS filter (intentionally incomplete — TB2 break-filter task)."""
import re
import sys


def remove_js(html: str) -> str:
    # Strip well-formed <script>...</script> (non-greedy, case-insensitive)
    html = re.sub(r"<\s*script\b[^>]*>.*?<\s*/\s*script\s*>", "", html, flags=re.I | re.S)
    # Strip self-closing / empty script tags
    html = re.sub(r"<\s*script\b[^>]*/\s*>", "", html, flags=re.I)
    # Remove common event handlers with quoted values
    html = re.sub(
        r"""\s+on\w+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)""",
        "",
        html,
        flags=re.I,
    )
    # Neutralize javascript: URLs in quoted attributes
    html = re.sub(r"""(href|src|action|formaction)\s*=\s*(['"])\s*javascript:""",
                  r"\1=\2", html, flags=re.I)
    # Remove iframe/object/embed opening tags (naive)
    for tag in ("iframe", "object", "embed", "frame"):
        html = re.sub(rf"<\s*{tag}\b[^>]*>", "", html, flags=re.I)
        html = re.sub(rf"<\s*/\s*{tag}\s*>", "", html, flags=re.I)
    return html


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("Usage: python filter.py <html_file>", file=sys.stderr)
        sys.exit(1)
    path = sys.argv[1]
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        content = f.read()
    with open(path, "w", encoding="utf-8") as f:
        f.write(remove_js(content))
