# edit-gauntlet (hard edit-tool bench)

Small Rust crate designed to **punish sloppy `edit` usage**. Logic is easy; matching and site selection are not.

## Goal

Make **`cargo test` pass** by fixing **only** the real bugs. Prefer the **`edit`** tool (surgical `old_string` → `new_string`). Avoid rewriting whole files with `write` unless edit truly cannot work.

## Required fixes (read carefully)

| Area | File | What is wrong | Correct behavior |
|------|------|---------------|------------------|
| Twins | `src/twins.rs` | `alpha_compute` and `beta_compute` both **add** | **alpha** must **multiply** (`a * b`); **beta** must **subtract** (`a - b`). `gamma_sum` / `delta_sum` must stay sums. |
| Crowded | `src/crowded.rs` | Many `n = n + 1` steps | Change **only** the step marked `TARGET_SITE_9` to `n = n * 3`. Other steps stay `+ 1`. |
| Whitespace | `src/whitespace.rs` | `scale_pair` wrong formula + trailing spaces / tab indent | Must compute `base * factor + y`. **Do not** mass-strip trailing spaces or reindent the file. |
| CRLF | `src/crlf_mod.rs` | `triple` adds; file is **CRLF** | Must `n * k`. **Keep CRLF** (do not convert the whole file to LF). |
| Rename | `src/rename_me.rs` | Type named `LegacyWidget` | Rename to **`CanonicalWidget` everywhere in this file**, including `format!("…#{}")` label prefix. |
| Unicode | `src/unicode.rs` | `score_line` adds 10 | Must multiply by 10 (`v * 10`). Keep smart quotes in `motto()` (`“gauntlet”`). |
| Nested | `src/deep/nested.rs` | `apply_bonus` adds | Must multiply. Leave `apply_noise` / `apply_padding` as sums. |
| Near twins | `src/near_twins.rs` | Three near-identical pipelines | Only **`primary_pipeline`** mid-step: `stage + 2` → `stage * 2`. Secondary/tertiary stay `+ 2`. |
| Bait | `src/bait.rs` | `combine` adds; comments/strings look the same | `combine` → `left * right`. **Do not** change the `DO_NOT_EDIT_COMMENT`, `help_text`, or `debug_label` strings. Leave `example_sum` as add. |
| Sandwich | `src/sandwich.rs` | Three `n + 1` lines | Only mid (`SANDWICH_MID`) → `n * 5`. Keep top/bot as `n + 1`. Leave `edge_pad` alone. |
| Expr | `src/expr.rs` | Wrong operators | `mixed` → `(a * b) - c` (not `a*b*c`, not `a+b-c`). Leave `mixed_decoy` as sum. In `scale_chain`, only mid `x + 10` → `x * 10`. |
| Shadow | `src/shadow.rs` | Same `let value = n + 1` twice | Change **only** the **inner** binding (`INNER_FIX`) to `n * 3`. Outer (`OUTER_KEEP`) stays `n + 1`. |

## Traps (agent failure modes we score against)

1. **Near-duplicate blocks** — same prologue in twins / near_twins; wrong twin or `replace_all` across both → fail.
2. **Crowded identical lines** — short `old_string` matches many times → need unique context or a precise site.
3. **Trailing whitespace / tabs** — exact matchers that drop them fail on `whitespace.rs` / `crlf_mod.rs`.
4. **CRLF** — LF-only `old_string` against CRLF file.
5. **Smart quotes** — U+201C/U+201D must survive.
6. **Decoys** — `src/decoy.rs` looks “buggy” but tests **require** current sums. Do not “fix” decoys.
7. **Comment / string bait** — `left + right` appears in comments and docs; only live code in `combine` changes.
8. **Sandwich lines** — three identical assignments; only the middle marker is the bug.
9. **Multi-operator expression** — changing the wrong `+` or all of them fails tests.
10. **Shadowed bindings** — same text in outer and inner scope; wrong site fails.
11. **Fingerprints** — tests assert markers (`ONLY ALPHA`, `TARGET_SITE_9`, `NEAR_TWIN_*`, `SANDWICH_MID`, …) and that `crlf_mod.rs` still contains `\r`. Whole-file rewrite/cleanup fails.
12. **Ambiguous short match** — after matching tighten, multi-match must not “guess”; include enough surrounding context.

## Constraints

- Prefer **`edit`** for every localized change; pass **`path` every time** (do not omit path on sibling tool calls).
- `new_string` must **differ** from `old_string` (no-op edits are errors).
- When a short fragment matches multiple times, **widen context** instead of using `replace_all` (except the intentional rename in `rename_me.rs`).
- Do not reformat the whole crate for style.
- Success = `cargo test` exit 0.
