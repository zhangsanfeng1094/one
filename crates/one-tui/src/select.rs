//! Reusable single/multi-select prompt for human-in-the-loop UIs
//! (tool permission, `ask_user`, …).
//!
//! Visual language matches Codex-style approval lists:
//! numbered options, radio `(•)/( )` or checkbox `[x]/[ ]`, footer shortcuts.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Single-choice radio list vs multi-select checkboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    Single,
    Multi,
}

/// One row in the option list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: String,
}

impl SelectOption {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: description.into(),
        }
    }
}

/// Input focus: navigating the list vs typing free-text / reject feedback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectPhase {
    List,
    /// Typing free-text answer or reject feedback.
    Typing {
        buffer: String,
    },
}

/// Outcome after the user confirms (or cancels) a select prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectResult {
    /// Confirmed selection. `ids` are option ids (empty when only free-text).
    Confirmed {
        ids: Vec<String>,
        other: Option<String>,
    },
    /// User cancelled (Esc / Ctrl+C without confirm).
    Cancelled,
}

/// Full select-prompt state machine (keyboard-driven).
#[derive(Debug, Clone)]
pub struct SelectPrompt {
    pub title: String,
    pub body: String,
    pub mode: SelectMode,
    pub options: Vec<SelectOption>,
    /// Focused row index (list phase).
    pub selected: usize,
    /// Checked option indices (multi mode).
    pub checked: HashSet<usize>,
    /// Allow free-text "Other" / reject feedback entry.
    pub allow_other: bool,
    /// Optional label for the free-text row (shown when `allow_other`).
    pub other_label: String,
    pub phase: SelectPhase,
    /// Footer hint override; empty → default by mode.
    pub footer_hint: String,
    /// When true, confirming with focus on a "deny/other" id enters typing phase
    /// instead of immediately confirming (used by permission reject).
    pub type_on_ids: HashSet<String>,
    /// Shortcut: Ctrl+O selects this option id immediately (permission always-approve).
    pub ctrl_o_id: Option<String>,
}

impl SelectPrompt {
    pub fn single(
        title: impl Into<String>,
        body: impl Into<String>,
        options: Vec<SelectOption>,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            mode: SelectMode::Single,
            options,
            selected: 0,
            checked: HashSet::new(),
            allow_other: false,
            other_label: "Other (type free text)".into(),
            phase: SelectPhase::List,
            footer_hint: String::new(),
            type_on_ids: HashSet::new(),
            ctrl_o_id: None,
        }
    }

    pub fn multi(
        title: impl Into<String>,
        body: impl Into<String>,
        options: Vec<SelectOption>,
    ) -> Self {
        let mut p = Self::single(title, body, options);
        p.mode = SelectMode::Multi;
        p
    }

    /// Permission dialog matching Codex-style list (4 options).
    ///
    /// When `reason` starts with `sandbox escalation:` (from PermissionGate),
    /// labels switch to Codex-style outside-sandbox approval copy, the body is
    /// restructured (why first, short command), and focus defaults to **once**
    /// instead of always-approve.
    pub fn permission(tool: &str, summary: &str, reason: &str) -> Self {
        let escalate = reason.starts_with("sandbox escalation:");
        let body = if escalate {
            format_escalate_body(tool, summary, reason)
        } else if reason.is_empty() {
            format!("{tool}\n{summary}")
        } else {
            format!("{tool}\n{summary}\n{reason}")
        };
        let options = if escalate {
            // Safer default first (Codex "Yes, proceed" is the primary action).
            // always-approve is available but not pre-focused.
            vec![
                SelectOption::new(
                    "once",
                    "Yes, run outside sandbox (this command only)",
                    "Disable bubblewrap for this single command",
                ),
                SelectOption::new(
                    "session",
                    "Yes, and don't ask again for this command",
                    "Remember escalate for this command until one exits",
                ),
                SelectOption::new(
                    "always",
                    "Yes, and don't ask again for anything",
                    "Auto-approve all tool asks (including escalations) for the rest of this process",
                ),
                SelectOption::new(
                    "deny",
                    "No, keep sandboxed",
                    "Do not escalate; optional message is sent to the model",
                ),
            ]
        } else {
            vec![
                SelectOption::new(
                    "always",
                    "Yes, and don't ask again for anything (always-approve mode)",
                    "Auto-approve all tool asks for the rest of this process",
                ),
                SelectOption::new("once", "Yes, proceed", "Allow this single tool call"),
                SelectOption::new(
                    "session",
                    "Yes, and don't ask again for this",
                    "Allow matching calls for the rest of this process",
                ),
                SelectOption::new(
                    "deny",
                    "No, reject (type to add feedback)",
                    "Block this call; optional message is sent to the model",
                ),
            ]
        };
        let title = if escalate {
            "Run outside sandbox?"
        } else {
            "Permission required"
        };
        let mut p = Self::single(title, body, options);
        // Escalate: focus "once" (index 0 after reorder). High-risk: keep always at 0.
        p.selected = 0;
        p.type_on_ids.insert("deny".into());
        p.ctrl_o_id = Some("always".into());
        p.footer_hint = if escalate {
            "↑↓/1-4:select  Enter:confirm  Ctrl+o:always-approve  Esc:deny".into()
        } else {
            "↑↓/1-4:select  Enter:confirm  Ctrl+o:always-approve  Esc:cancel".into()
        };
        p.other_label = "Feedback for the model (Enter empty to skip)".into();
        p
    }
}

/// Build a compact escalate body: why first, then a short command preview.
fn format_escalate_body(tool: &str, summary: &str, reason: &str) -> String {
    let why = reason
        .strip_prefix("sandbox escalation:")
        .unwrap_or(reason)
        .trim();
    // summary is often `[outside sandbox] <cmd>` or a description — peel prefix.
    let cmd = summary
        .strip_prefix("[outside sandbox]")
        .unwrap_or(summary)
        .trim();
    let cmd_preview = truncate_cmd_preview(cmd, 100);
    format!(
        "{tool} · leave bubblewrap for this command\n\
         Why: {why}\n\
         $ {cmd_preview}"
    )
}

fn truncate_cmd_preview(cmd: &str, max: usize) -> String {
    let one_line: String = cmd
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl SelectPrompt {
    pub fn option_count(&self) -> usize {
        let extra = if self.allow_other { 1 } else { 0 };
        self.options.len() + extra
    }

    pub fn is_other_row(&self, index: usize) -> bool {
        self.allow_other && index == self.options.len()
    }

    pub fn focused_option_id(&self) -> Option<&str> {
        if self.is_other_row(self.selected) {
            return Some("__other__");
        }
        self.options.get(self.selected).map(|o| o.id.as_str())
    }

    /// Default footer when `footer_hint` is empty.
    pub fn footer(&self) -> String {
        if !self.footer_hint.is_empty() {
            return self.footer_hint.clone();
        }
        match self.phase {
            SelectPhase::Typing { .. } => "Type text  Enter:submit  Esc:back".into(),
            SelectPhase::List => {
                let tab = if self.allow_other { "  Tab:other" } else { "" };
                match self.mode {
                    SelectMode::Single => {
                        let n = self.option_count().max(1);
                        format!(
                            "{}/{}:select{tab}  Enter:confirm  Esc:cancel",
                            self.selected + 1,
                            n
                        )
                    }
                    SelectMode::Multi => {
                        let n = self.option_count().max(1);
                        format!(
                            "{}/{}  Space:toggle{tab}  Enter:confirm  Esc:cancel",
                            self.selected + 1,
                            n
                        )
                    }
                }
            }
        }
    }

    pub fn move_up(&mut self) {
        if self.option_count() == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = self.option_count() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.option_count() == 0 {
            return;
        }
        self.selected = (self.selected + 1) % self.option_count();
    }

    pub fn select_index(&mut self, index: usize) {
        if index < self.option_count() {
            self.selected = index;
        }
    }

    /// Jump focus to the free-text "Other" row (when enabled).
    pub fn focus_other(&mut self) -> bool {
        if !self.allow_other {
            return false;
        }
        self.selected = self.options.len();
        true
    }

    /// Jump to Other and open the free-text typing phase (Tab shortcut).
    pub fn jump_to_other_typing(&mut self) -> bool {
        if !self.focus_other() {
            return false;
        }
        self.phase = SelectPhase::Typing {
            buffer: String::new(),
        };
        true
    }

    pub fn toggle_checked(&mut self) {
        if matches!(self.mode, SelectMode::Multi) && !self.is_other_row(self.selected) {
            if !self.checked.remove(&self.selected) {
                self.checked.insert(self.selected);
            }
        }
    }

    fn confirm_list(&mut self) -> Option<SelectResult> {
        // Free-text row or type-on ids → enter typing phase.
        if self.is_other_row(self.selected) {
            self.phase = SelectPhase::Typing {
                buffer: String::new(),
            };
            return None;
        }
        if let Some(id) = self.focused_option_id() {
            if self.type_on_ids.contains(id) {
                self.phase = SelectPhase::Typing {
                    buffer: String::new(),
                };
                return None;
            }
        }

        match self.mode {
            SelectMode::Single => {
                let id = self
                    .options
                    .get(self.selected)
                    .map(|o| o.id.clone())
                    .unwrap_or_default();
                Some(SelectResult::Confirmed {
                    ids: vec![id],
                    other: None,
                })
            }
            SelectMode::Multi => {
                let mut ids: Vec<String> = self
                    .checked
                    .iter()
                    .filter_map(|i| self.options.get(*i).map(|o| o.id.clone()))
                    .collect();
                // If nothing checked, treat focused option as selection when on a real option.
                if ids.is_empty() {
                    if let Some(o) = self.options.get(self.selected) {
                        ids.push(o.id.clone());
                    }
                }
                ids.sort();
                Some(SelectResult::Confirmed { ids, other: None })
            }
        }
    }

    fn confirm_typing(&mut self) -> SelectResult {
        let text = match &self.phase {
            SelectPhase::Typing { buffer } => buffer.trim().to_string(),
            SelectPhase::List => String::new(),
        };
        let other = if text.is_empty() { None } else { Some(text) };

        // Typing after focusing a type_on id (e.g. deny) or Other row.
        if self.is_other_row(self.selected) || self.allow_other && other.is_some() {
            // Free-text only (or multi with other).
            if matches!(self.mode, SelectMode::Multi) {
                let mut ids: Vec<String> = self
                    .checked
                    .iter()
                    .filter_map(|i| self.options.get(*i).map(|o| o.id.clone()))
                    .collect();
                ids.sort();
                return SelectResult::Confirmed { ids, other };
            }
            // Single: if we came from a type_on option (deny), include that id.
            if let Some(id) = self
                .options
                .get(self.selected)
                .map(|o| o.id.clone())
                .filter(|id| self.type_on_ids.contains(id))
            {
                return SelectResult::Confirmed {
                    ids: vec![id],
                    other,
                };
            }
            return SelectResult::Confirmed { ids: vec![], other };
        }

        // type_on option (deny with optional feedback)
        if let Some(id) = self.options.get(self.selected).map(|o| o.id.clone()) {
            return SelectResult::Confirmed {
                ids: vec![id],
                other,
            };
        }
        SelectResult::Cancelled
    }

    /// Handle one key event. Returns `Some` when the prompt is finished.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SelectResult> {
        match &self.phase {
            SelectPhase::Typing { .. } => self.handle_typing_key(key),
            SelectPhase::List => self.handle_list_key(key),
        }
    }

    /// Whether free-text typing currently owns keyboard focus.
    pub fn is_typing(&self) -> bool {
        matches!(self.phase, SelectPhase::Typing { .. })
    }

    /// Paste into the free-text buffer. Returns `true` if consumed (typing phase).
    pub fn handle_paste(&mut self, text: &str) -> bool {
        let SelectPhase::Typing { buffer } = &mut self.phase else {
            return false;
        };
        for ch in text.chars() {
            if !ch.is_control() {
                buffer.push(ch);
            }
        }
        true
    }

    fn handle_typing_key(&mut self, key: KeyEvent) -> Option<SelectResult> {
        match key.code {
            KeyCode::Esc => {
                self.phase = SelectPhase::List;
                None
            }
            KeyCode::Enter => Some(self.confirm_typing()),
            KeyCode::Backspace => {
                if let SelectPhase::Typing { buffer } = &mut self.phase {
                    buffer.pop();
                }
                None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let SelectPhase::Typing { buffer } = &mut self.phase {
                    buffer.push(c);
                }
                None
            }
            _ => None,
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Option<SelectResult> {
        // Ctrl+O → always-approve shortcut when configured.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('o') | KeyCode::Char('O') => {
                    if let Some(id) = &self.ctrl_o_id {
                        return Some(SelectResult::Confirmed {
                            ids: vec![id.clone()],
                            other: None,
                        });
                    }
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    return Some(SelectResult::Cancelled);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => Some(SelectResult::Cancelled),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
                None
            }
            KeyCode::Home => {
                self.selected = 0;
                None
            }
            KeyCode::End => {
                if self.option_count() > 0 {
                    self.selected = self.option_count() - 1;
                }
                None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                if n >= 1 && n <= self.option_count() {
                    self.select_index(n - 1);
                    // Digit alone only moves focus (Codex style: 1/3:select).
                    // Double-tap not required — Enter confirms.
                }
                None
            }
            KeyCode::Char(' ') if matches!(self.mode, SelectMode::Multi) => {
                self.toggle_checked();
                None
            }
            // Compatibility shortcuts for permission dialog.
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if self.options.iter().any(|o| o.id == "once") {
                    return Some(SelectResult::Confirmed {
                        ids: vec!["once".into()],
                        other: None,
                    });
                }
                self.confirm_list()
            }
            KeyCode::Char('a') | KeyCode::Char('A')
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if self.options.iter().any(|o| o.id == "session") {
                    return Some(SelectResult::Confirmed {
                        ids: vec!["session".into()],
                        other: None,
                    });
                }
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if self.options.iter().any(|o| o.id == "deny") {
                    // Jump to deny and enter feedback typing.
                    if let Some(idx) = self.options.iter().position(|o| o.id == "deny") {
                        self.selected = idx;
                    }
                    self.phase = SelectPhase::Typing {
                        buffer: String::new(),
                    };
                    return None;
                }
                Some(SelectResult::Cancelled)
            }
            // Tab → free-text Other (ask_user / any allow_other list).
            KeyCode::Tab => {
                let _ = self.jump_to_other_typing();
                None
            }
            KeyCode::Enter => self.confirm_list(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn sample_single() -> SelectPrompt {
        SelectPrompt::single(
            "Pick",
            "body",
            vec![
                SelectOption::new("a", "Alpha", "first"),
                SelectOption::new("b", "Beta", "second"),
                SelectOption::new("c", "Gamma", "third"),
            ],
        )
    }

    #[test]
    fn single_select_enter_confirms_focused() {
        let mut p = sample_single();
        p.move_down();
        let r = p.handle_key(key(KeyCode::Enter));
        assert_eq!(
            r,
            Some(SelectResult::Confirmed {
                ids: vec!["b".into()],
                other: None,
            })
        );
    }

    #[test]
    fn digit_moves_focus() {
        let mut p = sample_single();
        p.handle_key(key(KeyCode::Char('3')));
        assert_eq!(p.selected, 2);
        let r = p.handle_key(key(KeyCode::Enter));
        assert_eq!(
            r,
            Some(SelectResult::Confirmed {
                ids: vec!["c".into()],
                other: None,
            })
        );
    }

    #[test]
    fn multi_toggle_and_confirm() {
        let mut p = SelectPrompt::multi(
            "Multi",
            "pick many",
            vec![
                SelectOption::new("a", "A", ""),
                SelectOption::new("b", "B", ""),
                SelectOption::new("c", "C", ""),
            ],
        );
        p.handle_key(key(KeyCode::Char(' '))); // check a
        p.move_down();
        p.move_down();
        p.handle_key(key(KeyCode::Char(' '))); // check c
        let r = p.handle_key(key(KeyCode::Enter)).unwrap();
        match r {
            SelectResult::Confirmed { mut ids, other } => {
                ids.sort();
                assert_eq!(ids, vec!["a".to_string(), "c".to_string()]);
                assert!(other.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn permission_always_via_ctrl_o() {
        let mut p = SelectPrompt::permission("bash", "sudo id", "high-risk bash pattern `sudo`");
        let r = p.handle_key(key_ctrl(KeyCode::Char('o')));
        assert_eq!(
            r,
            Some(SelectResult::Confirmed {
                ids: vec!["always".into()],
                other: None,
            })
        );
    }

    #[test]
    fn sandbox_escalation_uses_outside_sandbox_labels() {
        let p = SelectPrompt::permission(
            "bash",
            "[outside sandbox] kill 1 && ps aux | grep auggie | head -20 | something very long",
            "sandbox escalation: cleanup host process",
        );
        assert_eq!(p.title, "Run outside sandbox?");
        // Safer default: "once" is first and focused.
        assert_eq!(p.options[0].id, "once");
        assert_eq!(p.selected, 0);
        assert_eq!(p.focused_option_id(), Some("once"));
        assert!(p.body.contains("Why: cleanup host process"), "{}", p.body);
        assert!(p.body.contains("$ "), "{}", p.body);
        // Long command is truncated for the dock.
        assert!(
            p.body.lines().any(|l| l.starts_with("$ ") && l.contains('…'))
                || p.body.lines().any(|l| l.starts_with("$ ") && l.len() <= 110),
            "expected short command preview, body:\n{}",
            p.body
        );
        assert!(p
            .options
            .iter()
            .any(|o| o.label.contains("run outside sandbox")));
    }

    #[test]
    fn truncate_cmd_preview_collapses_whitespace() {
        let s = truncate_cmd_preview("ps  aux\n|  grep x", 80);
        assert_eq!(s, "ps aux | grep x");
    }

    #[test]
    fn permission_deny_enters_typing_then_feedback() {
        let mut p = SelectPrompt::permission("bash", "rm -rf /", "high-risk");
        // Select deny (4)
        p.handle_key(key(KeyCode::Char('4')));
        assert_eq!(p.focused_option_id(), Some("deny"));
        let mid = p.handle_key(key(KeyCode::Enter));
        assert!(mid.is_none());
        assert!(matches!(p.phase, SelectPhase::Typing { .. }));
        p.handle_key(key(KeyCode::Char('n')));
        p.handle_key(key(KeyCode::Char('o')));
        let r = p.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(
            r,
            SelectResult::Confirmed {
                ids: vec!["deny".into()],
                other: Some("no".into()),
            }
        );
    }

    #[test]
    fn permission_y_a_shortcuts() {
        let mut p = SelectPrompt::permission("bash", "x", "r");
        assert_eq!(
            p.handle_key(key(KeyCode::Char('y'))),
            Some(SelectResult::Confirmed {
                ids: vec!["once".into()],
                other: None,
            })
        );
        let mut p = SelectPrompt::permission("bash", "x", "r");
        assert_eq!(
            p.handle_key(key(KeyCode::Char('a'))),
            Some(SelectResult::Confirmed {
                ids: vec!["session".into()],
                other: None,
            })
        );
    }

    #[test]
    fn cancel_esc() {
        let mut p = sample_single();
        assert_eq!(
            p.handle_key(key(KeyCode::Esc)),
            Some(SelectResult::Cancelled)
        );
    }

    #[test]
    fn tab_jumps_to_other_typing() {
        let mut p = sample_single();
        p.allow_other = true;
        assert!(!p.is_typing());
        let r = p.handle_key(key(KeyCode::Tab));
        assert!(r.is_none());
        assert!(p.is_other_row(p.selected));
        assert!(p.is_typing());
        p.handle_key(key(KeyCode::Char('h')));
        p.handle_key(key(KeyCode::Char('i')));
        let r = p.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(
            r,
            SelectResult::Confirmed {
                ids: vec![],
                other: Some("hi".into()),
            }
        );
    }

    #[test]
    fn tab_noop_without_other() {
        let mut p = sample_single();
        assert!(!p.allow_other);
        let r = p.handle_key(key(KeyCode::Tab));
        assert!(r.is_none());
        assert_eq!(p.selected, 0);
        assert!(!p.is_typing());
    }

    #[test]
    #[allow(dead_code)]
    fn key_event_kind_press_ok() {
        // Ensure we don't depend on release events.
        let ev = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        let mut p = sample_single();
        assert!(p.handle_key(ev).is_some());
    }
}
