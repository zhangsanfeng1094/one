# Stable TUI history browsing during streaming

## Problem

One's interactive transcript already has a `follow_bottom` flag, but its
`chat_scroll` value is counted from the bottom of the transcript. While the
user reads older output and an assistant response grows, the total rendered
line count increases while that bottom-relative value does not. The renderer
therefore advances the viewport start, pulling the reader towards the live
output on every redraw.

## Reference behaviour

DeepWiki analysis of `xai-org/grok-build` shows that its pager stores
`scroll_offset` as a row offset from the *top* of the scrollback. It has two
states:

1. Follow mode: layout places the viewport at `max_scroll_offset()` whenever
   content grows.
2. History browsing: the top-relative offset is retained while streaming
   content is appended, so the visible rows remain unchanged.

Grok Build returns to follow mode through `Shift+G` or a downward wheel
overscroll at the bottom.

## Approved One behaviour

- The existing `follow_bottom` boolean remains the two-state mode marker.
- While following, One renders the final viewport of the transcript.
- Any upward mouse-wheel or PageUp navigation enters history browsing. The
  saved scroll position is the absolute first visible transcript row, so
  streaming output added below it cannot move the viewport. This guarantee
  includes streaming-text finalization, thinking/tool transitions, and
  UI-only alerts emitted by the active agent turn.
- PageDown/wheel-down moves towards newer content. Reaching the bottom, or
  one additional downward wheel step at the bottom, re-enables following.
- `Shift+G` is an explicit jump-to-latest shortcut; it is unused by One today.
- A concise footer/status hint is shown only while browsing: `history ·
  Shift+G latest`.

## Scope and non-goals

This changes only `crates/one-tui`. It does not alter agent streaming,
session persistence, selection/copy semantics, shell scrollback, or the
welcome-screen layout. Terminal resize remains clamped to valid rows; a
semantic reflow anchor is not required to solve streaming jitter.

## Acceptance criteria

1. A reader who scrolls upward during assistant streaming sees the same
   transcript rows after subsequent streaming redraws and after that stream
   completes.
2. Live mode still exposes the latest lines of a multi-page response.
3. Scrolling down to the bottom, downward overscroll at bottom, and `Shift+G`
   each restore live follow mode.
4. A focused render test proves the stable viewport regression and all
   `one-tui` tests remain green.
