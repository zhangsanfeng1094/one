# Stable TUI history browsing implementation plan

> **For implementation:** use `@superpowers:test-driven-development` for each
> change, then `@superpowers:verification-before-completion` before handoff.

**Goal:** Make One's interactive TUI preserve a reader's viewport while an
agent streams new output, matching Grok Build's top-relative scrollback model.

**Architecture:** Retain `App.follow_bottom` as the explicit mode switch, but
make `App.chat_scroll` represent the absolute first visible transcript row
when browsing. The UI uses the computed maximum row as the view start only in
follow mode. App scrolling methods convert between the two modes; no streaming
event needs special-case scroll updates.

**Tech stack:** Rust 2021, Ratatui 0.29, Crossterm 0.28.

## Task 1: Lock the regression with focused TUI tests

**Files:**
- Modify: `crates/one-tui/src/ui.rs`
- Modify: `crates/one-tui/src/app.rs`

1. In the `ui.rs` test module, create a small helper that draws an `App` into
   `TestBackend` and returns the rendered symbols as `String`.
2. Add `history_viewport_stays_fixed_while_streaming_grows`:
   - Create a 40x14 terminal and enough early transcript lines to overflow the
     chat viewport.
   - Draw once, call `app.scroll_to_top()`, draw again, and retain
     `app.chat_view_start` plus an early marker visible in the buffer.
   - Append a uniquely marked streaming assistant chunk through the existing
     stream API, draw again, then assert both the early marker and the prior
     `chat_view_start` are unchanged. Assert `!app.follow_bottom`.
   - This must fail before implementation because the existing bottom-relative
     offset increases the renderer's computed view start as the stream grows.
3. Extend that test through `finish_stream_with_interrupted(false)` and one
   intervening tool/thinking transition. Assert the same early marker and
   `chat_view_start` remain visible after each transition. This must fail
   before Task 2 because finalization and several output-producing helpers
   currently call `scroll_to_bottom()` unconditionally.
4. In `app.rs` tests, add transition coverage:
   - after metrics establish a scrollable transcript, `scroll_to_top()` enters
     browse mode with top offset zero;
   - wheel/PageDown navigation at the bottom switches `follow_bottom` back on;
   - `Shift+G` calls `scroll_to_bottom()` and returns `RunOutcome::Noop` in
     both `handle_key` and `handle_busy_key`; use `KeyCode::Char('G')` with
     `KeyModifiers::SHIFT` to match Crossterm's normal representation.
5. Run the red tests:

   ```bash
   cargo test -p one-tui history_viewport_stays_fixed_while_streaming_grows
   cargo test -p one-tui shift_g_returns_to_live_follow
   ```

   Expected before Task 2: the viewport-stability test fails because the
   viewport marker or `chat_view_start` moves. The shortcut test fails because
   no `Shift+G` handler exists.

## Task 2: Switch browse state to a top-relative offset

**Files:**
- Modify: `crates/one-tui/src/app.rs`
- Modify: `crates/one-tui/src/ui.rs`

1. Update the `App.chat_scroll` documentation to state that it is the first
   visible display row while browsing; `follow_bottom=true` ignores its value
   and derives the start row from transcript height.
2. Replace the current bottom-relative navigation formulas in `App`:

   ```rust
   // enter browse mode from the live bottom, then move toward older rows
   pub fn scroll_up(&mut self, lines: usize) {
       let max = self.max_scroll();
       if self.follow_bottom {
           self.follow_bottom = false;
           self.chat_scroll = max;
       }
       self.chat_scroll = self.chat_scroll.saturating_sub(lines);
   }

   // move toward later rows; exact bottom resumes live updates
   pub fn scroll_down(&mut self, lines: usize) {
       if self.follow_bottom { return; }
       let max = self.max_scroll();
       self.chat_scroll = self.chat_scroll.saturating_add(lines).min(max);
       if self.chat_scroll == max && !self.messages.is_empty() {
           self.scroll_to_bottom();
       }
   }
   ```

   Make `scroll_to_top()` set `chat_scroll = 0` in browse mode. Keep
   `scroll_to_bottom()` as the single transition that sets follow mode and
   clears the stored offset.
3. In the transcript renderer, retain the existing line collection and clamp,
   but choose the first displayed row as:

   ```rust
   let start = if app.follow_bottom {
       max_from_bottom
   } else {
       app.chat_scroll.min(max_from_bottom)
   };
   ```

   Do not mutate `chat_scroll` on content growth while browsing. Continue
   assigning `app.chat_view_start = start` for selection/copy mapping.
4. Audit every transcript-output helper in `app.rs`. `push_assistant`,
   `push_system`, tool-call/tool-result updates, `push_alert`, thinking/text
   stream synchronization, and `finish_stream_with_interrupted` must preserve
   `follow_bottom=false`; they must no longer call `scroll_to_bottom()` just
   because agent output changed. `push_user` (a newly submitted user prompt)
   remains the deliberate transition that calls `scroll_to_bottom()`.
   A follow-mode redraw needs no imperative scroll call because Task 2's
   renderer already derives the latest start row from `max_from_bottom`.
5. Preserve the dedicated empty-welcome behavior: it must remain top-pinned
   and must not be treated as a live streaming transcript.
6. Run the tests from Task 1. They must pass before moving to Task 3.

## Task 3: Add Grok-style return-to-live controls and feedback

**Files:**
- Modify: `crates/one-tui/src/app.rs`
- Modify: `crates/one-tui/src/ui.rs`

1. Add a shared `is_goto_bottom_key(KeyEvent)` predicate that accepts
   Crossterm's normal `KeyCode::Char('G')` + `KeyModifiers::SHIFT` form (and
   the equivalent shifted `g` representation), while rejecting `Ctrl+G`.
   Call it before normal character insertion in both `handle_key` and
   `handle_busy_key`; it must call `scroll_to_bottom()` and leave `Ctrl+G`
   reserved for Settings.
2. Let `scroll_down()` treat a clamped bottom position as live follow. This
   makes a wheel-down event after browsing return to the stream without a
   separate mouse-only state machine.
3. Render a subdued, single-line `history · Shift+G latest` hint in the
   existing footer/status area only when `!follow_bottom` and the transcript is
   scrollable. It must not displace transcript rows or alter the input layout.
4. Add a `TestBackend` assertion that the hint is present only in browse mode.
5. Run:

   ```bash
   cargo test -p one-tui
   cargo fmt --check
   cargo clippy -p one-tui --all-targets -- -D warnings
   ```

## Task 4: Verify integration and document the keybinding

**Files:**
- Modify: `docs/cli.md`

1. In the interactive TUI keybinding/help section, document:
   `wheel/PgUp` browses stable history during streaming; `Shift+G` jumps to
   the live bottom; scrolling down to the bottom also resumes following.
2. Run the complete project checks:

   ```bash
   cargo test -p one-tui
   cargo test --workspace
   cargo fmt --check
   cargo clippy --workspace --all-targets -- -D warnings
   ```

3. Inspect `git diff --check` and `git diff -- crates/one-tui/src/app.rs
   crates/one-tui/src/ui.rs docs/cli.md` to ensure the change is limited to the
   approved behavior and has no whitespace errors.
