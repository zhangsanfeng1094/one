# FEAL Differential Cryptanalysis (Terminal-Bench 2.0 hard)

The file **`feal.py`** implements a FEAL-like encryption function. Implement a chosen-plaintext attack that recovers the value of **`key[5]`**.

Your attack must be in **`attack.py`** with:

```python
def attack(encrypt_fn) -> int:
    """Return the uint32 value of key[5]. Must finish in < 30 seconds."""
```

Notes:

- Each of the 6 round keys is derived from a 16-bit seed (`(seed * 1234567) & 0xFFFFFFFF`).
- You still cannot brute-force the entire keyspace, but a differential attack on the last round is feasible.
- Use only the provided `encrypt_fn` oracle (do not hard-code keys).

Scoring: `python3 verify.py`.
