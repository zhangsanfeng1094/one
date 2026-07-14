//! Persistent Ratatui session: enter once, draw each frame, leave on drop.

use std::fmt;
use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::Command;
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{App, RunOutcome};
use crate::error::Result;
use crate::ui;

/// DECSET 1007 — some terminals turn alt-screen wheel into arrow keys.
/// Kept as a fallback next to full mouse capture (VS Code / Cursor often
/// ignore 1007 and scroll shell history unless we capture the mouse).
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
/// OpenCode-like blink cadence for the software block caret.
const CURSOR_BLINK: Duration = Duration::from_millis(530);
/// Lines per mouse-wheel notch (trackpads may emit many notches).
const WHEEL_LINES: usize = 3;

/// Ratatui terminal session — whole interactive lifetime, not per keystroke.
pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    last_blink: Instant,
    restored: bool,
}

impl TerminalSession {
    /// Fullscreen setup: raw mode + alternate screen + mouse wheel capture.
    ///
    /// Alternate screen hides the main buffer (bash history). Mouse capture
    /// prevents the emulator from scrolling that buffer with the wheel and
    /// delivers `Event::Mouse` so the chat can scroll instead.
    ///
    /// Text selection: hold **Shift** and drag (standard for mouse-reporting TUIs).
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        // Clear alt buffer so no ghost of the primary screen remains.
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(EnableMouseCapture)?;
        stdout.execute(crossterm::event::EnableBracketedPaste)?;
        stdout.execute(EnableAlternateScroll)?;
        // Thin I-beam caret (not a fat block). Terminal handles the blink.
        stdout.execute(crossterm::cursor::SetCursorStyle::BlinkingBar)?;
        stdout.flush()?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            last_blink: Instant::now(),
            restored: false,
        })
    }

    pub fn draw(&mut self, app: &mut App) -> Result<()> {
        self.terminal.draw(|frame| ui::draw(frame, app))?;
        Ok(())
    }

    fn tick_blink(&mut self, app: &mut App) {
        if self.last_blink.elapsed() >= CURSOR_BLINK {
            app.toggle_cursor();
            self.last_blink = Instant::now();
        }
    }

    fn apply_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => app.scroll_down(WHEEL_LINES),
            // Left click in chat area → expand/collapse tool under cursor.
            MouseEventKind::Down(MouseButton::Left) => {
                // Chat fills all rows above the prompt/footer. Approximate:
                // prompt is ~3–9 rows + 1 footer at the bottom of the screen.
                let term_h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(0);
                let chat_h = app.chat_view_height as u16;
                if chat_h == 0 || term_h == 0 {
                    return;
                }
                // Chat is laid out at the top of the frame.
                if mouse.row < chat_h {
                    app.click_chat_row(mouse.row as usize);
                }
            }
            // Shift+drag selection is handled by the emulator when not capturing drag.
            _ => {}
        }
    }

    /// Block until the user submits an actionable outcome (prompt / quit / …).
    pub async fn wait_action(&mut self, app: &mut App) -> Result<RunOutcome> {
        loop {
            self.tick_blink(app);
            // Only re-stick when already following — never undo PageUp here.
            self.draw(app)?;

            if event::poll(POLL_IDLE)? {
                match event::read()? {
                    Event::Key(key) => {
                        // Crossterm may emit Press + Release; App filters non-Press.
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                            let outcome = app.handle_key(key);
                            if outcome.is_actionable() {
                                return Ok(outcome);
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        Self::apply_mouse(app, mouse);
                    }
                    Event::Paste(text) => {
                        app.handle_paste(&text);
                    }
                    Event::Resize(_, _) => {
                        // Next draw uses the new size.
                    }
                    _ => {}
                }
            }

            tokio::task::yield_now().await;
        }
    }

    /// Redraw while an async agent task runs; `on_tick` drains external events into `app`.
    pub async fn run_busy<T>(
        &mut self,
        app: &mut App,
        mut on_tick: impl FnMut(&mut App),
        done: tokio::task::JoinHandle<T>,
    ) -> T {
        loop {
            on_tick(app);
            app.sync_stream_message();
            self.tick_blink(app);
            let _ = self.draw(app);

            if done.is_finished() {
                break done.await.expect("agent task panicked");
            }

            if let Ok(true) = event::poll(POLL_BUSY) {
                match event::read() {
                    Ok(Event::Key(key)) => {
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                            app.handle_busy_key(key);
                        }
                    }
                    Ok(Event::Mouse(mouse)) => {
                        Self::apply_mouse(app, mouse);
                    }
                    Ok(Event::Paste(text)) => {
                        app.handle_paste(&text);
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
        let _ = backend.execute(crossterm::cursor::SetCursorStyle::DefaultUserShape);
        let _ = backend.execute(DisableMouseCapture);
        let _ = backend.execute(DisableAlternateScroll);
        let _ = backend.execute(crossterm::event::DisableBracketedPaste);
        let _ = backend.execute(LeaveAlternateScreen);
        disable_raw_mode()?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
