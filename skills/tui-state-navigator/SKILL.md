---
name: tui-state-navigator
description: Use this skill when validating, exploring, or repairing terminal TUI workflows with tmux captures, especially when button/order changes can break keyboard navigation. It learns and stores state transitions in a project-local JSON state map.
---

# TUI State Navigator

Use this skill for terminal UIs where a visual state must be checked through real keyboard interaction rather than by reading code alone.

The skill is generic. It can drive any terminal TUI that can run inside tmux, as long as the project provides a state map JSON with a launch command, state classifiers, transitions, and safe learning candidates.

The core pattern is:

1. Run the TUI inside tmux.
2. Capture the screen.
3. Classify the current screen into a named state using visible markers.
4. Choose a transition toward the target state.
5. Send keys.
6. Capture and verify the predicted next state.
7. If a transition fails, learn a replacement key sequence and store it in the state map.

## Project Tools

In a project that bundles this skill, prefer these tools:

- `scripts/tui-state-navigator.js`: state-machine navigator with optional learning.
- a state map JSON, for example `scripts/tui-state-map.json`: persisted command, state definitions, transitions, markers, and candidate key sequences.

## Standard Workflow

Start with a full current-state check. If the project map contains `command`, no `--cmd` is needed:

```bash
node scripts/tui-state-navigator.js --all --wait 1 --key-wait 0.05 --width 120 --height 32
```

For a specific target:

```bash
node scripts/tui-state-navigator.js --target float.settings --trace --wait 1 --key-wait 0.05
```

When focus/highlight matters, include ANSI capture and focus reporting:

```bash
node scripts/tui-state-navigator.js --target float.settings --trace --ansi --show-focus
```

When a target fails because a menu option moved, run learning mode:

```bash
node scripts/tui-state-navigator.js --target float.settings --learn --trace --wait 1 --key-wait 0.05
```

Learning mode tries candidate key sequences from the source state. If one reaches the expected target state, it updates the selected state map with the new `keys`, `learned: true`, and `learnedAt`.

After learning, inspect the diff before trusting it:

```bash
git diff -- scripts/tui-state-map.json
node scripts/tui-state-navigator.js --all --wait 1 --key-wait 0.05
```

## Adding New States

If the screen is classified as `unknown`, add a state entry to the active state map.

Use stable visible markers:

- Prefer panel titles, page titles, and persistent labels.
- Avoid values from local user config, counts, timestamps, paths, wrapped phrases, or selected row names.
- Put specific states before broad overview states.
- Use `exclude` markers when an overview page shares the same title as modal/editor states.

Then add or learn transitions into that state.

## Safety

Learning mode should only be used on non-destructive navigation states unless the state map candidate list has been constrained.

Do not add broad candidates like `Enter` on screens where the highlighted action may save, delete, export, launch, or trigger external actions. For those states, add explicit safe candidates under `candidates.<state>` in the JSON map.

## Output Expectations

When reporting results, include:

- Target(s) checked.
- Whether all targets reached.
- Any learned transition and the exact new key sequence.
- Any state-map files changed.
- Any remaining unknown or unsafe states.

For detailed schema notes, see `references/state-map-schema.md`.
