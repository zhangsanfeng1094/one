# edit-gauntlet — maintainer solutions (do not ship into agent workspace)

## Intent

Hard **edit-tool** bench: many easy logic bugs behind matching traps. Scoring cares about:

- `cargo test` green
- fingerprints preserved (no mass rewrite)
- agent used `edit` (trace)
- low tool errors / bounded turns

New traps (easy to botch):

| Trap | Wrong agent move |
|------|------------------|
| near_twins | short `stage + 2` / block-anchor hits secondary/tertiary |
| bait | `replace_all` on `left + right` rewrites comment/docs |
| sandwich | changes all three `n + 1` or the wrong line |
| expr | `a * b * c` or only first `+` → still fails |
| shadow | edits outer `value` instead of inner |

## Minimal diffs

### `twins.rs`

```diff
 // ONLY ALPHA: product
-    left + right
+    left * right
```

```diff
 // ONLY BETA: difference
-    left + right
+    left - right
```

### `crowded.rs`

Only the `TARGET_SITE_9` assignment:

```diff
     // TARGET_SITE_9: must become n * 3 (not n + 1)
-    n = n + 1;
+    n = n * 3;
```

### `whitespace.rs`

```diff
-    base + factor + y
+    base * factor + y
```

Keep trailing spaces / tab on the `let` lines.

### `crlf_mod.rs`

```diff
-    n + k // BUG: must be n * k
+    n * k // BUG: must be n * k
```

Preserve CRLF.

### `rename_me.rs`

`replace_all` (or careful multi-edit) `LegacyWidget` → `CanonicalWidget` including the format string prefix.

### `unicode.rs`

```diff
-    format!("val={}", v + 10)
+    format!("val={}", v * 10)
```

Keep `“gauntlet”` smart quotes.

### `deep/nested.rs`

```diff
-    base + bonus
+    base * bonus
```

only inside `apply_bonus`.

### `near_twins.rs`

Include the PRIMARY marker in `old_string`:

```diff
     // NEAR_TWIN_PRIMARY_MID
-    let mid = stage + 2;
+    let mid = stage * 2;
```

Do **not** touch secondary/tertiary.

### `bait.rs`

```diff
     // BAIT_CODE_LINE
-    left + right
+    left * right
```

Leave comment, `help_text`, `debug_label`, `example_sum` alone.

### `sandwich.rs`

```diff
     // SANDWICH_MID: must become n * 5 (not n + 1)
-    let mid = n + 1;
+    let mid = n * 5;
```

### `expr.rs`

```diff
     // EXPR_MIXED_TARGET
-    a + b + c
+    a * b - c
```

```diff
     // EXPR_CHAIN_MID: must be x * 10
-    let y = x + 10;
+    let y = x * 10;
```

### `shadow.rs`

```diff
-        let value = n + 1; // INNER_FIX: must become n * 3
+        let value = n * 3; // INNER_FIX: must become n * 3
```

Outer `OUTER_KEEP` stays `n + 1`.

## Offline check (no LLM)

```bash
# baseline should fail
cp -a benches/tasks/edit-gauntlet/fixture /tmp/eg && cd /tmp/eg && cargo test

# after applying solutions, cargo test passes
```

## Run with agent

```bash
./benches/run.sh full edit-gauntlet
./benches/run.sh full edit-gauntlet --provider ziyong-gpt   # example
```
