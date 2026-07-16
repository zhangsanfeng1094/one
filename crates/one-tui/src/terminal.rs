//! Persistent Ratatui session: enter once, draw each frame, leave on drop.
//!
//! ## Mouse scroll vs copy (what actually works)
//!
//! Terminal protocol cannot give *native* free drag-select while mouse tracking
//! is on (emulator stops owning selection). DeepWiki survey of lazygit /
//! bubbletea / helix / Claude Code:
//!
//! 1. **Keep mouse capture** so the wheel scrolls the TUI, not shell history.
//! 2. **In-app selection** on drag (highlight lines in the chat).
//! 3. **OSC 52** push to the system clipboard on release (also keybinding).
//! 4. Optional: Shift still releases capture for hosts that want native select.
//!
//! Free drag-select without Shift only works if the *application* implements
//! selection — which we do.

use std::fmt;
use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::Command;
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{App, RunOutcome};
use crate::clipboard;
use crate::error::Result;
use crate::ui;

/// Ctrl+C during a busy turn — leave interactive mode immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForceQuit;

/// Mouse modes for wheel + multi-line drag select.
///
/// - `?1000` press/release + wheel
/// - `?1002` cell motion **while button held** (required for multi-line drag;
///   without it most hosts never send `Drag` between Down and Up)
/// - `?1006` SGR coordinates
///
/// Deliberately **not** `?1003` (any-motion without button) — that steals hover
/// and makes free selection feel broken.
#[derive(Debug, Clone, Copy)]
struct EnableBasicMouse;

impl Command for EnableBasicMouse {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }
}

#[derive(Debug, Clone, Copy)]
struct DisableBasicMouse;

impl Command for DisableBasicMouse {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1006l\x1b[?1002l\x1b[?1000l")
    }
}

#[derive(Debug, Clone, Copy)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007h")
    }
}

#[derive(Debug, Clone, Copy)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007l")
    }
}

const POLL_IDLE: Duration = Duration::from_millis(50);
const POLL_BUSY: Duration = Duration::from_millis(40);
const CURSOR_BLINK: Duration = Duration::from_millis(530);
const WHEEL_LINES: usize = 3;
const SELECT_RELEASE_REARM: Duration = Duration::from_millis(800);

fn mouse_capture_default() -> bool {
    match std::env::var("ONE_MOUSE")
        .ok()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("0" | "off" | "false" | "no") => false,
        Some("1" | "on" | "true" | "yes") => true,
        _ => true,
    }
}

/// Ratatui terminal session — whole interactive lifetime, not per keystroke.
pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    last_blink: Instant,
    restored: bool,
    mouse_want: bool,
    mouse_armed: bool,
    select_release_at: Option<Instant>,
    /// Left button currently down in chat (in-app select).
    left_down: bool,
}

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        let mouse_want = mouse_capture_default();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(Clear(ClearType::All))?;
        apply_input_modes(&mut stdout, mouse_want)?;
        stdout.execute(crossterm::cursor::Hide)?;
        stdout.flush()?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            last_blink: Instant::now(),
            restored: false,
            mouse_want,
            mouse_armed: mouse_want,
            select_release_at: None,
            left_down: false,
        })
    }

    fn reassert_modes(&mut self) {
        let armed = self.mouse_want && self.select_release_at.is_none();
        let _ = apply_input_modes(self.terminal.backend_mut(), armed);
        self.mouse_armed = armed;
    }

    fn release_mouse_for_native_select(&mut self) {
        if self.mouse_armed {
            let _ = self.terminal.backend_mut().execute(DisableBasicMouse);
            self.mouse_armed = false;
        }
        self.select_release_at = Some(Instant::now());
    }

    fn arm_mouse_if_wanted(&mut self) {
        self.select_release_at = None;
        if self.mouse_want && !self.mouse_armed {
            let _ = self.terminal.backend_mut().execute(EnableBasicMouse);
            self.mouse_armed = true;
        }
    }

    fn maybe_rearm_after_select(&mut self) {
        let Some(at) = self.select_release_at else {
            return;
        };
        if at.elapsed() >= SELECT_RELEASE_REARM {
            self.arm_mouse_if_wanted();
        }
    }

    fn toggle_mouse(&mut self, app: &mut App) {
        self.mouse_want = !self.mouse_want;
        self.select_release_at = None;
        if self.mouse_want {
            let _ = self.terminal.backend_mut().execute(EnableBasicMouse);
            self.mouse_armed = true;
            app.set_notice("mouse on · drag to copy · wheel scrolls chat");
        } else {
            let _ = self.terminal.backend_mut().execute(DisableBasicMouse);
            self.mouse_armed = false;
            app.set_notice("mouse off · terminal drag-select · pgup/pgdn scroll");
        }
        app.mouse_capture = self.mouse_want;
    }

    /// Flush pending clipboard payload immediately (OSC 52 + host fallbacks).
    fn flush_clipboard(&mut self, app: &mut App) {
        if let Some(text) = app.clipboard_pending.take() {
            let lines = text.lines().count().max(1);
            let n = text.chars().count();
            // WSL: clip.exe UTF-16LE; else OSC 52 / wl-copy / …
            match clipboard::copy_text(self.terminal.backend_mut(), &text) {
                Ok(()) => {
                    if lines > 1 {
                        app.set_notice(format!("copied {lines} lines ({n} chars)"));
                    } else {
                        app.set_notice(format!("copied {n} chars"));
                    }
                }
                Err(e) => {
                    app.set_notice(format!("copy failed · {e}"));
                }
            }
        }
    }

    pub fn draw(&mut self, app: &mut App) -> Result<()> {
        // Finalize clipboard image pastes before paint so chips leave "loading".
        app.poll_image_jobs();
        app.mouse_capture = self.mouse_want;
        self.terminal.draw(|frame| ui::draw(frame, app))?;
        self.flush_clipboard(app);
        Ok(())
    }

    fn tick_blink(&mut self, app: &mut App) -> bool {
        // Also poll here so jobs complete even when idle without redraw churn.
        app.poll_image_jobs();
        if self.last_blink.elapsed() >= CURSOR_BLINK {
            app.toggle_cursor();
            self.last_blink = Instant::now();
            true
        } else {
            false
        }
    }

    fn apply_mouse(&mut self, app: &mut App, mouse: crossterm::event::MouseEvent) {
        // Shift = optional native terminal selection (xterm convention).
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            match mouse.kind {
                MouseEventKind::Down(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::Moved
                | MouseEventKind::Up(_) => {
                    self.left_down = false;
                    self.release_mouse_for_native_select();
                }
                MouseEventKind::ScrollUp => app.scroll_up(WHEEL_LINES),
                MouseEventKind::ScrollDown => app.scroll_down(WHEEL_LINES),
                _ => {}
            }
            return;
        }

        if self.select_release_at.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::Down(_) => {
                    self.arm_mouse_if_wanted();
                }
                _ => {}
            }
        }

        // Chat pane height (rows above prompt/footer).
        let chat_h = app.chat_view_height as u16;
        let in_chat = chat_h > 0 && mouse.row < chat_h;
        let row = mouse.row as usize;

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.left_down = false;
                app.scroll_up(WHEEL_LINES);
            }
            MouseEventKind::ScrollDown => {
                self.left_down = false;
                app.scroll_down(WHEEL_LINES);
            }
            MouseEventKind::Down(MouseButton::Left) if in_chat => {
                self.left_down = true;
                app.select_begin(row);
            }
            // Multi-line select: Drag + Moved while held (hosts vary).
            MouseEventKind::Drag(MouseButton::Left) if self.left_down => {
                if in_chat {
                    app.select_update(row, true);
                }
            }
            MouseEventKind::Moved if self.left_down => {
                if in_chat {
                    app.select_update(row, true);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.left_down {
                    self.left_down = false;
                    // select_finish applies release row; drag/multi-line → auto-copy.
                    app.select_finish(row);
                    self.flush_clipboard(app);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Click outside chat clears selection.
                self.left_down = false;
                app.clear_selection();
            }
            _ => {}
        }
    }

    fn handle_key_global(&mut self, app: &mut App, key: crossterm::event::KeyEvent) -> bool {
        // Ctrl+Shift+M → toggle mouse capture.
        if matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            self.toggle_mouse(app);
            return true;
        }
        // Ctrl+Shift+C or plain `y` when selection active → OSC 52 copy.
        // (Plain Ctrl+C is progressive dismiss / double-tap quit in App.)
        if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            app.request_copy_selection();
            self.flush_clipboard(app);
            return true;
        }
        if matches!(key.code, KeyCode::Char('y'))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && app.selection_range().is_some()
            && app.input.is_empty()
        {
            app.request_copy_selection();
            self.flush_clipboard(app);
            return true;
        }
        false
    }

    pub async fn wait_action(&mut self, app: &mut App) -> Result<RunOutcome> {
        // Mouse tips live on the empty-state footer / help — do not spam a
        // floating toast every idle wait (steals focus from the prompt).

        loop {
            self.maybe_rearm_after_select();
            self.tick_blink(app);
            self.draw(app)?;

            if event::poll(POLL_IDLE)? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                            if self.handle_key_global(app, key) {
                                continue;
                            }
                            if self.select_release_at.is_some() {
                                self.arm_mouse_if_wanted();
                            }
                            let outcome = app.handle_key(key);
                            if outcome.is_actionable() {
                                return Ok(outcome);
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        self.apply_mouse(app, mouse);
                    }
                    Event::Paste(text) => {
                        app.handle_paste(&text);
                    }
                    Event::Resize(_, _) => {
                        self.reassert_modes();
                    }
                    _ => {}
                }
            }

            tokio::task::yield_now().await;
        }
    }

    pub async fn run_busy<T>(
        &mut self,
        app: &mut App,
        mut on_tick: impl FnMut(&mut App),
        done: tokio::task::JoinHandle<T>,
    ) -> std::result::Result<T, ForceQuit> {
        loop {
            self.maybe_rearm_after_select();
            on_tick(app);
            app.sync_stream_message();
            self.tick_blink(app);
            let _ = self.draw(app);

            if app.take_force_quit() {
                done.abort();
                match tokio::time::timeout(Duration::from_millis(750), done).await {
                    Ok(_) => {}
                    Err(_) => {}
                }
                return Err(ForceQuit);
            }

            if done.is_finished() {
                on_tick(app);
                app.sync_stream_message();
                let _ = self.draw(app);
                return Ok(done.await.expect("agent task panicked"));
            }

            if let Ok(true) = event::poll(POLL_BUSY) {
                match event::read() {
                    Ok(Event::Key(key)) => {
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                            if self.handle_key_global(app, key) {
                                continue;
                            }
                            if self.select_release_at.is_some() {
                                self.arm_mouse_if_wanted();
                            }
                            app.handle_busy_key(key);
                            if app.force_quit_pending() {
                                continue;
                            }
                        }
                    }
                    Ok(Event::Mouse(mouse)) => {
                        self.apply_mouse(app, mouse);
                    }
                    Ok(Event::Paste(text)) => {
                        app.handle_paste(&text);
                    }
                    Ok(Event::Resize(_, _)) => {
                        self.reassert_modes();
                    }
                    _ => {}
                }
            }

            tokio::task::yield_now().await;
        }
    }

    pub fn leave(mut self) -> Result<()> {
        self.restore()
    }

    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let backend = self.terminal.backend_mut();
        if self.mouse_armed || self.mouse_want {
            let _ = backend.execute(DisableBasicMouse);
        }
        let _ = backend.execute(DisableAlternateScroll);
        let _ = backend.execute(crossterm::event::DisableBracketedPaste);
        let _ = backend.execute(LeaveAlternateScreen);
        disable_raw_mode()?;
        let _ = backend.execute(crossterm::cursor::Show);
        self.terminal.show_cursor()?;
        Ok(())
    }
}

fn apply_input_modes<W: Write>(w: &mut W, mouse_capture: bool) -> io::Result<()> {
    if mouse_capture {
        w.execute(EnableBasicMouse)?;
    } else {
        let _ = w.execute(DisableBasicMouse);
    }
    w.execute(crossterm::event::EnableBracketedPaste)?;
    w.execute(EnableAlternateScroll)?;
    Ok(())
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
