#!/usr/bin/env python3
"""Verify filter-js-from-html without Selenium (static XSS heuristics)."""
from __future__ import annotations
import re
import subprocess
import sys
import tempfile
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent

ATTACKS = [
    "<script>alert(1)</script>",
    "< script >alert(2)</ script >",
    '<img src=x onerror="alert(3)">',
    '<body onload="alert(4)">',
    '<a href="javascript:alert(5)">Click</a>',
    '<svg onload="alert(7)"></svg>',
    '<meta http-equiv="refresh" content="0;url=javascript:alert(9)">',
    '<object data="javascript:alert(10)"></object>',
    '<embed src="javascript:alert(11)">',
    '<input autofocus onfocus="alert(12)">',
    "<ScRiPt>alert(15)</ScRiPt>",
    '<form action="javascript:alert(19)"><input type=submit></form>',
]

SAFE_HTML = """<!DOCTYPE html>
<html><head><title>OK</title></head>
<body>
  <h1>Hello</h1>
  <p class="lead">Safe <strong>text</strong>.</p>
  <table><tr><td>1</td></tr></table>
  <a href="https://example.com">link</a>
  <img src="photo.png" alt="x">
</body></html>
"""

def _run_filter(path: Path) -> None:
    r = subprocess.run(
        [sys.executable, str(ROOT / "filter.py"), str(path)],
        capture_output=True, text=True, cwd=str(ROOT),
    )
    assert r.returncode == 0, f"filter.py failed: {r.stderr}\n{r.stdout}"

def _looks_dangerous(html: str) -> bool:
    low = html.lower()
    # script tags that are not escaped
    if re.search(r"<\s*script\b", low):
        return True
    if "javascript:" in low:
        return True
    # event handlers
    if re.search(r"\son\w+\s*=", low):
        return True
    return False

def test_filter_exists() -> None:
    assert (ROOT / "filter.py").is_file()

def test_strips_attack_vectors() -> None:
    for i, attack in enumerate(ATTACKS):
        with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
            # wrap attack in a page
            f.write(f"<html><body><p>prefix</p>{attack}<p>suffix</p></body></html>")
            path = Path(f.name)
        try:
            _run_filter(path)
            out = path.read_text(errors="replace")
            assert not _looks_dangerous(out), (
                f"Attack {i} still looks executable after filter:\n  in={attack!r}\n  out={out!r}"
            )
        finally:
            path.unlink(missing_ok=True)

def test_preserves_safe_html() -> None:
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
        f.write(SAFE_HTML)
        path = Path(f.name)
    try:
        _run_filter(path)
        out = path.read_text()
        assert "Hello" in out
        assert "Safe" in out
        assert "example.com" in out
        assert "photo.png" in out
        assert "lead" in out or "class" in out
    finally:
        path.unlink(missing_ok=True)

def main() -> int:
    tests = [test_filter_exists, test_strips_attack_vectors, test_preserves_safe_html]
    failed = 0
    for fn in tests:
        try:
            fn(); print(f"PASS  {fn.__name__}")
        except Exception as e:
            failed += 1
            print(f"FAIL  {fn.__name__}: {e}")
            traceback.print_exc()
    print()
    if failed:
        print(f"{failed}/{len(tests)} failed"); return 1
    print(f"all {len(tests)} passed"); return 0

if __name__ == "__main__":
    raise SystemExit(main())
