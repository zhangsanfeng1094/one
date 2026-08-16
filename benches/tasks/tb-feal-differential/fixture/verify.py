#!/usr/bin/env python3
"""Verify feal-differential attack recovers key[5]."""
from __future__ import annotations
import importlib.util
import sys
import time
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(mod)
    return mod

def test_feal_differential_cryptanalysis_attack() -> None:
    feal = _load("feal", ROOT / "feal.py")
    attack_mod = _load("attack", ROOT / "attack.py")
    assert hasattr(attack_mod, "attack"), "attack.py must define attack(encrypt_fn)"

    # Re-seed keys randomly each run
    feal.create_random_keys()
    expected = feal.get_keys()[5] if hasattr(feal, "get_keys") else feal.key[5]

    # Wrap encrypt so attack cannot simply read feal.key
    real_encrypt = feal.encrypt
    def oracle(pt: int) -> int:
        return real_encrypt(pt)

    t0 = time.time()
    stolen = attack_mod.attack(oracle)
    elapsed = time.time() - t0
    assert elapsed < 30.0, f"attack took {elapsed:.1f}s (>30s)"
    assert int(stolen) == int(expected), f"key[5] expected {expected}, got {stolen}"

def main() -> int:
    try:
        test_feal_differential_cryptanalysis_attack()
        print("PASS  test_feal_differential_cryptanalysis_attack")
        print("all 1 passed")
        return 0
    except Exception as e:
        print(f"FAIL  test_feal_differential_cryptanalysis_attack: {e}")
        traceback.print_exc()
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
