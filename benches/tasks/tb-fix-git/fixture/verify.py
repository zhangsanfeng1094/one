#!/usr/bin/env python3
"""Verify fix-git: about.md and default.html match patch_files."""
from __future__ import annotations
import hashlib
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent

def hash_file(path: Path) -> str:
    return hashlib.md5(path.read_bytes().strip()).hexdigest()

def test_about_file() -> None:
    old = ROOT / "resources/patch_files/about.md"
    new = ROOT / "personal-site/_includes/about.md"
    assert new.is_file(), f"missing {new}"
    assert hash_file(old) == hash_file(new), f"about.md not recovered correctly"

def test_layout_file() -> None:
    old = ROOT / "resources/patch_files/default.html"
    new = ROOT / "personal-site/_layouts/default.html"
    assert new.is_file(), f"missing {new}"
    assert hash_file(old) == hash_file(new), f"default.html not recovered correctly"

def test_on_master() -> None:
    # Soft check: HEAD should be master (or main) after merge
    head = (ROOT / "personal-site/.git/HEAD").read_text().strip()
    assert "master" in head or "main" in head, f"HEAD not on master/main: {head}"

def main() -> int:
    tests = [test_about_file, test_layout_file, test_on_master]
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
