#!/usr/bin/env python3
"""Verify break-filter-js-from-html without Selenium."""
from __future__ import annotations
import re
import shutil
import subprocess
import sys
import tempfile
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent

def test_out_html_bypasses_filter() -> None:
    out_path = ROOT / "out.html"
    assert out_path.is_file(), "out.html missing"
    original = out_path.read_text(errors="replace")
    assert original.strip(), "out.html is empty"

    with tempfile.TemporaryDirectory() as td:
        td_p = Path(td)
        filtered = td_p / "out.html"
        filtered.write_text(original)
        # Apply the vulnerable filter (same as environment/filter.py)
        r = subprocess.run(
            [sys.executable, str(ROOT / "filter.py"), str(filtered)],
            capture_output=True, text=True,
        )
        assert r.returncode == 0, f"filter failed: {r.stderr}"
        result = filtered.read_text(errors="replace")

    # Static proxy for "alert still fires": after filtering, a live script
    # tag or javascript: URL or on* handler must remain.
    low = result.lower()
    has_script = re.search(r"<\s*script\b", low) is not None
    has_js_url = "javascript:" in low
    has_handler = re.search(r"\son\w+\s*=", low) is not None
    has_alert = "alert(" in low or "alert (" in low
    assert has_script or has_js_url or has_handler, (
        "No executable JS residual after filtering. Bypass failed.\n"
        f"filtered={result!r}"
    )
    # Prefer that payload still looks like an alert (oracle uses alert(401))
    if has_script:
        assert has_alert or "eval(" in low or "document." in low, (
            "Script tag survived but no obvious payload (alert/eval/document)"
        )

def main() -> int:
    try:
        test_out_html_bypasses_filter()
        print("PASS  test_out_html_bypasses_filter")
        print("all 1 passed")
        return 0
    except Exception as e:
        print(f"FAIL  test_out_html_bypasses_filter: {e}")
        traceback.print_exc()
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
