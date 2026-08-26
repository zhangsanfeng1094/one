//! Persistent Ratatui session: enter once, draw each frame, leave on drop.
//!
//! ## Mouse scroll vs copy (what actually works)
//!
//! Terminal protocol cannot give *native* free drag-select while mouse tracking
//! is on (emulator stops owning selection). DeepWiki survey of lazygit /
//! bubbletea / helix / Claude Code:
//!
//! 1. **Keep mouse capture** so the wheel scrolls the TUI, not shell history.
//! 2. **In-app selection** on drag (character-level highlight in the chat).
//! 3. **OSC 52** push to the system clipboard on release (also keybinding).
//! 4. Optional: Shift still releases capture for hosts that want native select.
//!
//! Free drag-select without Shift only works if the *application* implements
//! selection — which we do (caret-based half-open range per display line).

use std::fmt;
use std::future::Future;
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

use crate::app::App;
use crate::clipboard;
use crate::error::Result;
use crate::paste_burst::{coalesce_events, looks_like_paste_burst, CoalescedEvent};
use crate::state::RunOutcome;
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
/// Max events pulled in one drain so a huge unbracketed paste cannot stall forever.
const DRAIN_MAX: usize = 32_768;
/// After a paste-like burst, wait this long for straggler chunks before painting.
const PASTE_STRAGGLER: Duration = Duration::from_millis(8);

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

        // Chat pane in absolute terminal rows (below the grok header / sticky).
        let chat_h = app.chat_view_height as u16;
        let in_chat = app.mouse_to_chat_row(mouse.row).is_some();
        // Relative to the painted transcript; saturating so a drag onto the
        // header still hits row 0 (edge-scroll up).
        let row = mouse.row.saturating_sub(app.chat_content_y);
        let col = mouse.column;

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.left_down = false;
                // Float (e.g. /tasks live log) steals the wheel from chat.
                if app.has_float() {
                    app.scroll_float_wheel(true, WHEEL_LINES);
                } else {
                    app.scroll_up(WHEEL_LINES);
                }
            }
            MouseEventKind::ScrollDown => {
                self.left_down = false;
                if app.has_float() {
                    app.scroll_float_wheel(false, WHEEL_LINES);
                } else {
                    app.scroll_down(WHEEL_LINES);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_chat => {
                self.left_down = true;
                app.select_begin(row as usize, col);
            }
            // Character / multi-line select: Drag + Moved while held (hosts vary).
            // Keep tracking even when the pointer leaves the chat pane so the
            // user can edge-scroll past one viewport of lines.
            MouseEventKind::Drag(MouseButton::Left) if self.left_down => {
                app.select_drag(row, col, chat_h);
            }
            MouseEventKind::Moved if self.left_down => {
                app.select_drag(row, col, chat_h);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.left_down {
                    self.left_down = false;
                    // select_finish applies release cell; non-empty drag → auto-copy.
                    app.select_finish_at(row, col, chat_h);
                    self.flush_clipboard(app);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Click outside chat clears selection. If clicked on sticky bar, jump to message start.
                self.left_down = false;
                app.clear_selection();
                app.click_sticky(mouse.row);
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
            && app.has_selection()
            && app.input.is_empty()
        {
            app.request_copy_selection();
            self.flush_clipboard(app);
            return true;
        }
        false
    }

    pub async fn wait_action(&mut self, app: &mut App) -> Result<RunOutcome> {
        self.wait_action_with(app, |_| {}).await
    }

    /// Like [`wait_action`], but `on_poll` runs every idle frame (~50ms).
    /// Used to refresh live status chips (e.g. MCP 4/5) without a keypress.
    pub async fn wait_action_with(
        &mut self,
        app: &mut App,
        mut on_poll: impl FnMut(&mut App),
    ) -> Result<RunOutcome> {
        // Mouse tips live on the empty-state footer / help — do not spam a
        // floating toast every idle wait (steals focus from the prompt).

        loop {
            on_poll(app);
            self.maybe_rearm_after_select();
            self.tick_blink(app);

            // Skip the idle draw while a paste burst is already in the queue so
            // we never paint thousands of intermediate frames.
            let ready = event::poll(Duration::ZERO)?;
            if !ready {
                self.draw(app)?;
                if !event::poll(POLL_IDLE)? {
                    tokio::task::yield_now().await;
                    continue;
                }
            }

            for ev in coalesce_events(drain_events()?) {
                match ev {
                    CoalescedEvent::Key(key) => {
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
                    CoalescedEvent::Mouse(mouse) => self.apply_mouse(app, mouse),
                    CoalescedEvent::Paste(text) => app.handle_paste(&text),
                    CoalescedEvent::Resize(_, _) => self.reassert_modes(),
                    CoalescedEvent::Other => {}
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

            let ready = event::poll(Duration::ZERO).unwrap_or(false);
            if !ready {
                let _ = self.draw(app);
                match event::poll(POLL_BUSY) {
                    Ok(true) => {}
                    _ => {
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            }

            if let Ok(events) = drain_events() {
                for ev in coalesce_events(events) {
                    match ev {
                        CoalescedEvent::Key(key) => {
                            if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                                if self.handle_key_global(app, key) {
                                    continue;
                                }
                                if self.select_release_at.is_some() {
                                    self.arm_mouse_if_wanted();
                                }
                                app.handle_busy_key(key);
                            }
                        }
                        CoalescedEvent::Mouse(mouse) => self.apply_mouse(app, mouse),
                        CoalescedEvent::Paste(text) => app.handle_paste(&text),
                        CoalescedEvent::Resize(_, _) => self.reassert_modes(),
                        CoalescedEvent::Other => {}
                    }
                }
            }

            tokio::task::yield_now().await;
        }
    }

    /// Keep painting the TUI while `work` runs on this task (no spawn).
    ///
    /// Used for `/compact`: the LLM summary holds `&mut AppRuntime`, so it
    /// cannot be `tokio::spawn`'d next to the event loop.
    pub async fn run_until<T>(
        &mut self,
        app: &mut App,
        mut on_tick: impl FnMut(&mut App),
        work: impl Future<Output = T>,
    ) -> std::result::Result<T, ForceQuit> {
        tokio::pin!(work);
        loop {
            self.maybe_rearm_after_select();
            on_tick(app);
            app.sync_stream_message();
            self.tick_blink(app);

            if app.take_force_quit() {
                return Err(ForceQuit);
            }

            tokio::select! {
                biased;
                result = &mut work => {
                    on_tick(app);
                    app.sync_stream_message();
                    let _ = self.draw(app);
                    return Ok(result);
                }
                _ = self.pump_busy_events(app) => {}
            }
        }
    }

    async fn pump_busy_events(&mut self, app: &mut App) {
        let ready = event::poll(Duration::ZERO).unwrap_or(false);
        if !ready {
            let _ = self.draw(app);
            match event::poll(POLL_BUSY) {
                Ok(true) => {}
                _ => {
                    tokio::task::yield_now().await;
                    return;
                }
            }
        }

        if let Ok(events) = drain_events() {
            for ev in coalesce_events(events) {
                match ev {
                    CoalescedEvent::Key(key) => {
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                            if self.handle_key_global(app, key) {
                                continue;
                            }
                            if self.select_release_at.is_some() {
                                self.arm_mouse_if_wanted();
                            }
                            app.handle_busy_key(key);
                        }
                    }
                    CoalescedEvent::Mouse(mouse) => self.apply_mouse(app, mouse),
                    CoalescedEvent::Paste(text) => app.handle_paste(&text),
                    CoalescedEvent::Resize(_, _) => self.reassert_modes(),
                    CoalescedEvent::Other => {}
                }
            }
        }

        tokio::task::yield_now().await;
    }

    pub fn leave(mut self) -> Result<()> {
        self.restore()
    }

    /// Temporarily leave alternate screen + raw mode so the user can use a normal
    /// terminal (e.g. OAuth login with stdin prompts). Call [`resume`] after.
    pub fn suspend(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let backend = self.terminal.backend_mut();
        if self.mouse_armed || self.mouse_want {
            let _ = backend.execute(DisableBasicMouse);
            self.mouse_armed = false;
        }
        let _ = backend.execute(DisableAlternateScroll);
        let _ = backend.execute(crossterm::event::DisableBracketedPaste);
        let _ = backend.execute(LeaveAlternateScreen);
        disable_raw_mode()?;
        let _ = backend.execute(crossterm::cursor::Show);
        let _ = self.terminal.show_cursor();
        let _ = io::stdout().flush();
        // Mark restored so Drop won't double-tear-down if we panic mid-suspend;
        // resume() clears this flag.
        self.restored = true;
        Ok(())
    }

    /// Re-enter alternate screen after [`suspend`].
    pub fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let backend = self.terminal.backend_mut();
        backend.execute(EnterAlternateScreen)?;
        backend.execute(Clear(ClearType::All))?;
        apply_input_modes(backend, self.mouse_want)?;
        self.mouse_armed = self.mouse_want;
        backend.execute(crossterm::cursor::Hide)?;
        let _ = backend.flush();
        self.terminal.clear()?;
        self.restored = false;
        self.select_release_at = None;
        self.left_down = false;
        // Drop any key/mouse events queued while suspended (login typing, etc.).
        while event::poll(Duration::from_millis(0)).unwrap_or(false) {
            let _ = event::read();
        }
        Ok(())
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

/// Read every event already in the kernel buffer (plus a short wait if this
/// looks like a clipboard dump) so paste is one block, not one key per frame.
fn drain_events() -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    events.push(event::read()?);
    loop {
        while events.len() < DRAIN_MAX && event::poll(Duration::ZERO)? {
            events.push(event::read()?);
        }
        if events.len() >= DRAIN_MAX {
            break;
        }
        if looks_like_paste_burst(&events) && event::poll(PASTE_STRAGGLER)? {
            continue;
        }
        break;
    }
    Ok(events)
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

/// Best-effort leave raw mode / alternate screen when the process is dying
/// (panic hook). Safe to call when the terminal was never entered.
///
/// Prefer [`TerminalSession::restore`] on the normal exit path — this only
/// exists so a panic mid-TUI does not leave the user's shell unusable, and so
/// a subsequent `eprintln!` of the panic log path is actually visible.
pub fn emergency_restore_terminal() {
    // Order mirrors `restore`: mouse/paste off → leave alt screen → disable raw → show cursor.
    let mut out = io::stdout();
    let _ = out.execute(DisableBasicMouse);
    let _ = out.execute(DisableAlternateScroll);
    let _ = out.execute(crossterm::event::DisableBracketedPaste);
    let _ = out.execute(LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = out.execute(crossterm::cursor::Show);
    let _ = out.flush();
}
