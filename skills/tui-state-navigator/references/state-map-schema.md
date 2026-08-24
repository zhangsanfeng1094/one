# TUI State Map Schema

The navigator reads a JSON map passed with `--map`. In this repository the default is `scripts/tui-state-map.json`, but the same schema works for other TUIs.

## Top-Level Fields

- `version`: schema version.
- `command`: optional TUI launch command. If omitted, pass `--cmd`.
- `cwd`: optional working directory. Relative paths are resolved from the map file's directory.
- `start`: initial state expected after launching the command.
- `states`: ordered state classifier definitions.
- `transitions`: directed navigation edges.
- `candidates`: fallback key sequences used by `--learn`.
- `updatedAt`: set by the navigator when it saves learned changes.

Minimal map:

```json
{
  "version": 1,
  "command": "your-tui-command",
  "cwd": ".",
  "start": "home",
  "states": [
    {"name": "home", "include": ["Home"]}
  ],
  "transitions": []
}
```

For a different TUI, change `command`, `cwd`, `start`, state markers, and transitions. The navigator itself does not require specific application labels.

## State

```json
{
  "name": "float.settings.tool_output",
  "include": ["Tool output", "Max lines"],
  "exclude": []
}
```

Classification checks states in array order. A state matches when every `include` marker is visible and no `exclude` marker is visible.

Put specific child states before overview states that share a title. For example, `float.settings.tool_output` should be checked before `float.settings` if both contain `Settings`.

## Transition

```json
{
  "from": "home",
  "to": "float.settings",
  "keys": ["C-g"],
  "markers": ["Ctrl+G settings"],
  "learned": true,
  "learnedAt": "2026-08-24T00:00:00.000Z"
}
```

- `from`: source state.
- `to`: predicted destination state.
- `keys`: tmux key names to send in order.
- `markers`: source-state labels that must be visible before sending keys.
- `selectedText`: optional text expected in the currently highlighted/focused region before sending keys.
- `learned`: optional flag set when `--learn` repairs the edge.
- `learnedAt`: optional timestamp for learned updates.

Markers should prove that the option still exists. Keep them short and stable; long wrapped text can produce false failures.

`selectedText` is useful when the same option labels are visible but the focused row matters. The navigator captures ANSI style with tmux, extracts likely highlighted text, and checks that `selectedText` appears in that focus text. If no styled focus can be detected, it falls back to plain-screen presence.

## Candidates

```json
{
  "candidates": {
    "default": [["Enter"], ["Down", "Enter"], ["C-g"], ["Esc"]],
    "float.settings": [["Down", "Enter"], ["Up", "Enter"], ["Enter"], ["Esc"]]
  }
}
```

When a transition fails in `--learn` mode, the navigator tries candidates for the source state first, then `default`.

Use narrow candidate lists for states with destructive actions. Avoid `Enter` as a candidate when the current selection can save, delete, export, or execute destructive actions.
