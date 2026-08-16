#!/usr/bin/env python3
"""Verify polyglot-c-py (adapted paths)."""
from __future__ import annotations
import subprocess
import sys
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent

def test_fibonacci_polyglot() -> None:
    poly = ROOT / "polyglot"
    assert poly.is_dir(), "polyglot/ missing"
    files = sorted(p.name for p in poly.iterdir() if p.is_file() and not p.name.startswith("."))
    src_only = [f for f in files if f.endswith((".py", ".c", ".h", ".cpp", ".cc", ".hpp", ".py.c"))]
    # main.py.c ends with .c but is the polyglot name
    assert "main.py.c" in files, f"main.py.c missing, found: {files}"
    # Disallow extra source helpers (compiled binaries ok)
    extra_src = [f for f in src_only if f != "main.py.c"]
    assert not extra_src, f"Expected only main.py.c source, also found: {extra_src}"

    main = poly / "main.py.c"
    c_bin = poly / "cmain"
    c_bin.unlink(missing_ok=True)

    r = subprocess.run(["gcc", str(main), "-o", str(c_bin)], capture_output=True, text=True)
    assert r.returncode == 0, f"gcc failed: {r.stderr}"

    cases = [(42, "267914296"), (10, "55"), (0, "0")]
    for n, expected in cases:
        py = subprocess.run(
            [sys.executable, str(main), str(n)], capture_output=True, text=True, check=True
        ).stdout.strip()
        c = subprocess.run(
            [str(c_bin), str(n)], capture_output=True, text=True, check=True
        ).stdout.strip()
        assert py == expected and c == expected, (
            f"f({n}): expected {expected}, got Python={py!r}, C={c!r}"
        )

def main() -> int:
    try:
        test_fibonacci_polyglot()
        print("PASS  test_fibonacci_polyglot")
        print("all 1 passed")
        return 0
    except Exception as e:
        print(f"FAIL  test_fibonacci_polyglot: {e}")
        traceback.print_exc()
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
