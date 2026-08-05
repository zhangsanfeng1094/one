//! Input buffer editing: caret, chips (image/text), paste, path completion.

use std::path::PathBuf;

use crate::state::{PendingImage, PendingText};

use super::helpers::{list_path_completions, longest_common_prefix, path_token_at_end};
use super::ImagePasteJob;

impl super::App {
    pub fn take_pending_images(&mut self) -> Vec<(String, String)> {
        if !self.committed_images.is_empty() {
            return std::mem::take(&mut self.committed_images);
        }
        // Fallback: still in the input (e.g. tests calling take without submit).
        self.sync_pending_chips();
        let order = one_core::image::image_token_ids_in(&self.input);
        let mut by_id: std::collections::HashMap<u32, PendingImage> = self
            .pending_images
            .drain(..)
            .map(|img| (img.id, img))
            .collect();
        order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .map(|img| (img.mime_type, img.path.display().to_string()))
            .collect()
    }

    /// Drop chips whose token was deleted from the input.
    pub fn sync_pending_chips(&mut self) {
        let imgs: std::collections::HashSet<u32> = one_core::image::image_token_ids_in(&self.input)
            .into_iter()
            .collect();
        self.pending_images.retain(|img| imgs.contains(&img.id));
        let texts: std::collections::HashSet<u32> = one_core::image::text_token_ids_in(&self.input)
            .into_iter()
            .collect();
        self.pending_texts.retain(|t| texts.contains(&t.id));
    }

    /// Backward-compatible alias.
    pub fn sync_pending_images(&mut self) {
        self.sync_pending_chips();
    }

    pub(crate) fn clamp_input_cursor(&mut self) {
        let len = self.input.chars().count();
        if self.input_cursor > len {
            self.input_cursor = len;
        }
    }

    /// Move caret to end of `input` (history recall, bulk replace, chip append).
    pub(crate) fn input_cursor_end(&mut self) {
        self.input_cursor = self.input.chars().count();
    }

    pub(crate) fn input_cursor_home(&mut self) {
        self.input_cursor = 0;
    }

    /// Byte index in `input` for the current char cursor.
    pub(crate) fn input_byte_at_cursor(&self) -> usize {
        self.input
            .chars()
            .take(self.input_cursor)
            .map(|c| c.len_utf8())
            .sum()
    }

    /// Left/right caret movement inside the main prompt.
    pub fn move_input_cursor(&mut self, delta: isize) {
        let len = self.input.chars().count() as isize;
        let next = (self.input_cursor as isize + delta).clamp(0, len);
        self.input_cursor = next as usize;
        self.cursor_on = true;
    }

    /// Split `input` at the caret for rendering: (before, after).
    pub fn input_split_at_cursor(&self) -> (&str, &str) {
        let idx = self.input_byte_at_cursor().min(self.input.len());
        let idx = if self.input.is_char_boundary(idx) {
            idx
        } else {
            self.input.len()
        };
        (&self.input[..idx], &self.input[idx..])
    }

    /// Insert a character at the caret.
    pub(crate) fn insert_input_char(&mut self, ch: char) {
        if ch.is_control() && ch != '\n' {
            return;
        }
        self.clear_chat_focus();
        self.clamp_input_cursor();
        let idx = self.input_byte_at_cursor();
        self.input.insert(idx, ch);
        self.input_cursor += 1;
        self.cursor_on = true;
    }

    /// Insert a string at the caret (control chars other than `\n` dropped).
    pub(crate) fn insert_input_str(&mut self, text: &str) {
        let cleaned: String = text
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        if cleaned.is_empty() {
            return;
        }
        self.clear_chat_focus();
        self.clamp_input_cursor();
        let idx = self.input_byte_at_cursor();
        let n = cleaned.chars().count();
        self.input.insert_str(idx, &cleaned);
        self.input_cursor += n;
        self.cursor_on = true;
    }

    pub(crate) fn insert_chip_token(&mut self, token: &str) {
        // Prefer a leading space when inserting mid-buffer after non-whitespace.
        self.clear_chat_focus();
        self.clamp_input_cursor();
        let idx = self.input_byte_at_cursor();
        let need_lead = idx > 0
            && !self.input[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let mut piece = String::new();
        if need_lead {
            piece.push(' ');
        }
        piece.push_str(token);
        piece.push(' ');
        let n = piece.chars().count();
        self.input.insert_str(idx, &piece);
        self.input_cursor += n;
        self.cursor_on = true;
    }

    /// Attach a ready local image file and insert `[图片.img]` into the input.
    pub fn attach_image_path(&mut self, mime_type: String, path: PathBuf, name: String) {
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.saturating_add(1).max(2);
        let token = one_core::image::image_token(id);
        let label = one_core::image::image_label_path(&mime_type, &path);
        self.pending_images.push(PendingImage {
            id,
            mime_type,
            path,
            name: name.clone(),
            loading: false,
        });
        self.insert_chip_token(&token);
        self.set_notice(format!("attached {name}  {token}  {label}"));
    }

    /// Attach from base64: write into media store, then path-based attach.
    pub fn attach_image(&mut self, mime_type: String, data: String, name: String) {
        match one_core::image::store_image_base64(&data, Some(&mime_type)) {
            Ok((path, mime)) => self.attach_image_path(mime, path, name),
            Err(err) => self.set_notice(format!("image attach failed: {err}")),
        }
    }

    /// True while any image chip is still loading from clipboard / import.
    pub fn has_loading_images(&self) -> bool {
        self.pending_images.iter().any(|i| i.loading) || !self.image_jobs.is_empty()
    }

    /// Insert an optimistic loading chip immediately (before disk/clipboard work).
    pub(crate) fn begin_loading_image(&mut self, name: &str) -> u32 {
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.saturating_add(1).max(2);
        let token = one_core::image::image_token(id);
        self.pending_images.push(PendingImage {
            id,
            mime_type: "image/*".into(),
            path: PathBuf::new(),
            name: name.to_string(),
            loading: true,
        });
        self.insert_chip_token(&token);
        self.cursor_on = true;
        self.set_notice(format!("pasting {token}…"));
        id
    }

    /// Remove a chip + pending entry by id (failed paste / user abandoned load).
    pub(crate) fn remove_pending_image(&mut self, id: u32) {
        self.pending_images.retain(|i| i.id != id);
        let token = one_core::image::image_token(id);
        if let Some(pos) = self.input.find(&token) {
            let mut start = pos;
            let mut end = pos + token.len();
            // insert_chip_token writes ` token ` — peel one trailing space.
            if self.input[end..].starts_with(' ') {
                end += 1;
            }
            // And one leading space when not at start.
            if start > 0 && self.input.as_bytes().get(start - 1) == Some(&b' ') {
                start -= 1;
            }
            let removed = self.input[start..end].chars().count();
            let caret_byte = self.input_byte_at_cursor();
            self.input.replace_range(start..end, "");
            if caret_byte >= end {
                self.input_cursor = self.input_cursor.saturating_sub(removed);
            } else if caret_byte > start {
                // Caret was inside the chip — snap to the cut point.
                self.input_cursor = self.input[..start].chars().count();
            }
            self.clamp_input_cursor();
        }
    }

    /// Apply a finished load onto an existing loading chip (or drop on error).
    pub(crate) fn finish_loading_image(
        &mut self,
        id: u32,
        result: Result<(String, PathBuf, String), String>,
        report_err: bool,
    ) {
        match result {
            Ok((mime, path, name)) => {
                if let Some(img) = self.pending_images.iter_mut().find(|i| i.id == id) {
                    img.mime_type = mime;
                    img.path = path;
                    img.name = name.clone();
                    img.loading = false;
                    let label = img.label();
                    let token = img.token();
                    self.set_notice(format!("attached {name}  {token}  {label}"));
                }
            }
            Err(err) => {
                self.remove_pending_image(id);
                if report_err {
                    self.set_notice(format!(
                        "paste failed · {err} · copy a screenshot, or paste a path"
                    ));
                } else {
                    // Quiet probe (e.g. empty bracketed paste) — chip already removed.
                    self.clear_notice();
                }
            }
        }
    }

    /// Poll background image jobs; call every frame from the terminal loop.
    pub fn poll_image_jobs(&mut self) {
        if self.image_jobs.is_empty() {
            return;
        }
        let mut still = Vec::new();
        let jobs = std::mem::take(&mut self.image_jobs);
        for job in jobs {
            match job.rx.try_recv() {
                Ok(result) => {
                    self.finish_loading_image(job.id, result, job.report_err);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    still.push(job);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.finish_loading_image(
                        job.id,
                        Err("image paste cancelled".into()),
                        job.report_err,
                    );
                }
            }
        }
        self.image_jobs = still;
    }

    /// Try host-clipboard bitmap paste (WSL PowerShell / wl-paste / xclip).
    ///
    /// Shows a loading chip **immediately**, then fills path in the background
    /// so the UI never freezes on PowerShell / host tools.
    ///
    /// Returns `true` when a job was started (chip visible). Call
    /// [`Self::poll_image_jobs`] each frame to finalize.
    pub fn try_paste_clipboard_image(&mut self, report_err: bool) -> bool {
        // Don't steal focus from select/float free-text.
        if self.select.is_some() || self.float_open() {
            return false;
        }
        let id = self.begin_loading_image("clipboard.png");
        let (tx, rx) = std::sync::mpsc::channel();
        self.image_jobs.push(ImagePasteJob { id, report_err, rx });
        std::thread::spawn(move || {
            let _ = tx.send(crate::clipboard::paste_image());
        });
        true
    }

    /// Fast check: text looks like an existing image file path (no copy yet).
    pub(crate) fn quick_image_path_candidate(&self, text: &str) -> Option<PathBuf> {
        let path = crate::clipboard::normalize_pasted_path(text).or_else(|| {
            let t = text.trim().trim_matches(|c| c == '"' || c == '\'');
            Some(PathBuf::from(t))
        })?;
        if !one_core::image::is_image_path(&path) {
            return None;
        }
        path.is_file().then_some(path)
    }

    /// Resolve pasted text to an imported media image `(mime, path, name)`.
    pub(crate) fn load_image_from_pasted_path_static(text: &str) -> Option<(String, PathBuf, String)> {
        if let Some(v) = one_core::image::try_load_image_path_paste(text) {
            return Some(v);
        }
        let path = crate::clipboard::normalize_pasted_path(text)?;
        let s = path.to_str()?;
        one_core::image::try_load_image_path_paste(s)
    }

    /// Collapse a long paste into `[文本.txt]` (body kept until submit / delete).
    pub fn attach_text_blob(&mut self, body: String) {
        let id = self.next_text_id;
        self.next_text_id = self.next_text_id.saturating_add(1).max(2);
        let token = one_core::image::text_token(id);
        let summary = one_core::image::text_blob_summary(&body);
        self.pending_texts.push(PendingText { id, body });
        self.insert_chip_token(&token);
        self.set_notice(format!("pasted  {token}  {summary}"));
    }

    /// Backspace: delete one character (or an entire paste chip) before the caret.
    pub fn pop_input(&mut self) {
        if self.input_cursor == 0 || self.input.is_empty() {
            return;
        }
        let byte_end = self.input_byte_at_cursor();
        let before = &self.input[..byte_end];
        if let Some(n) = one_core::image::paste_chip_backspace_len(before) {
            let start = byte_end.saturating_sub(n);
            // How many chars were removed so the caret can step back correctly.
            let removed_chars = self.input[start..byte_end].chars().count();
            self.input.replace_range(start..byte_end, "");
            self.input_cursor = self.input_cursor.saturating_sub(removed_chars);
        } else {
            // Remove the single char immediately before the caret.
            self.input_cursor -= 1;
            let idx = self.input_byte_at_cursor();
            if idx < self.input.len() {
                self.input.remove(idx);
            }
        }
        self.clamp_input_cursor();
        self.sync_pending_chips();
        self.cursor_on = true;
        self.clear_notice();
    }

    /// Delete: remove one character (or paste chip) at/after the caret.
    pub fn delete_input_forward(&mut self) {
        if self.input_cursor >= self.input.chars().count() {
            return;
        }
        let idx = self.input_byte_at_cursor();
        let rest = &self.input[idx..];
        // Atomic chip delete when caret sits at the start of `[图片…]` / `[文本…]`.
        let chip_len = one_core::image::parse_image_token_at(rest)
            .map(|(_, len)| len)
            .or_else(|| one_core::image::parse_text_token_at(rest).map(|(_, len)| len));
        if let Some(len) = chip_len {
            let mut end = idx + len;
            // Peel optional trailing space that insert_chip_token adds.
            if self.input[end..].starts_with(' ') {
                end += 1;
            }
            self.input.replace_range(idx..end, "");
        } else if let Some(ch) = rest.chars().next() {
            self.input.remove(idx);
            let _ = ch;
        }
        self.clamp_input_cursor();
        self.sync_pending_chips();
        self.cursor_on = true;
        self.clear_notice();
    }

    pub fn handle_paste(&mut self, text: &str) {
        // Docked select free-text phase owns paste (never the main prompt).
        if let Some(prompt) = self.select.as_mut() {
            if prompt.handle_paste(text) {
                self.cursor_on = true;
                self.clear_notice();
                return;
            }
            // List phase: swallow paste so it does not leak into the main input.
            return;
        }

        // Center float (Settings field edit / search filter) owns paste.
        if self.float_open() {
            if let Some(f) = self.float.as_mut() {
                f.paste_search(text);
            }
            self.cursor_on = true;
            self.clear_notice();
            return;
        }

        // Empty bracketed paste: terminal cannot deliver bitmaps — try host clipboard
        // (screenshot / browser copy-as-image). Codex does the same via keybind.
        if text.trim().is_empty() {
            // Quiet: no error toast if clipboard has no image (chip removed on fail).
            let _ = self.try_paste_clipboard_image(false);
            self.cursor_on = true;
            return;
        }

        // data-URI → optimistic chip, media write on a worker thread.
        if let Some((mime, data)) = one_core::image::parse_data_uri(text) {
            let id = self.begin_loading_image("paste.png");
            let (tx, rx) = std::sync::mpsc::channel();
            self.image_jobs.push(ImagePasteJob {
                id,
                report_err: true,
                rx,
            });
            std::thread::spawn(move || {
                let r = one_core::image::store_image_base64(&data, Some(&mime))
                    .map(|(path, mime)| (mime, path, "paste.png".into()));
                let _ = tx.send(r);
            });
            return;
        }
        // Path paste: bare / quoted / file:// / Windows→WSL — chip first, import async.
        if let Some(src) = self.quick_image_path_candidate(text) {
            let name = src
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image")
                .to_string();
            let id = self.begin_loading_image(&name);
            let (tx, rx) = std::sync::mpsc::channel();
            self.image_jobs.push(ImagePasteJob {
                id,
                report_err: true,
                rx,
            });
            let text = text.to_string();
            std::thread::spawn(move || {
                let r = Self::load_image_from_pasted_path_static(&text)
                    .ok_or_else(|| "not an image path".to_string());
                let _ = tx.send(r);
            });
            return;
        }

        // Preserve newlines for multi-line paste (normalize \r\n / \r → \n).
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        // Long paste → `[文本.txt]` chip (same UX as images: compact + atomic delete).
        if one_core::image::should_collapse_paste(&normalized) {
            // Drop other control chars from the stored body.
            let body: String = normalized
                .chars()
                .filter(|c| *c == '\n' || !c.is_control())
                .collect();
            self.attach_text_blob(body);
            self.cursor_on = true;
            return;
        }

        self.insert_input_str(&normalized);
        self.clear_notice();
    }

    /// Whether the main prompt owns interaction focus (and thus the blinking caret).
    ///
    /// Matches Grok / common TUI practice: caret only while the composer is the
    /// active pane. Hidden when:
    /// - a float modal or select dock owns the keyboard, or
    /// - empty-prompt j/k transcript browse (`chat_focus`) owns visual focus
    ///   (blue rail on a history row — typing returns to the prompt).
    pub fn prompt_focused(&self) -> bool {
        self.select.is_none()
            && !self.float_open()
            && !self.transcript_browse_focused()
    }

    /// Empty-prompt transcript browse (j/k / click focus rail). Keys like j/k
    /// navigate history; printable keys re-enter the composer.
    pub fn transcript_browse_focused(&self) -> bool {
        self.chat_focus.is_some() && self.input.is_empty()
    }

    /// Clear transcript row focus when the user returns to typing in the prompt.
    pub(crate) fn clear_chat_focus(&mut self) {
        self.chat_focus = None;
    }

    /// How many visual lines the prompt input currently needs (capped).
    pub fn input_line_count(&self) -> usize {
        let n = self.input.split('\n').count().max(1);
        n.min(6)
    }

    pub fn complete_path_token(&mut self) {
        let Some((prefix, partial)) = path_token_at_end(&self.input) else {
            return;
        };
        let matches = list_path_completions(&partial);
        if matches.is_empty() {
            self.set_notice(format!("no match for `{partial}`"));
            return;
        }
        if matches.len() == 1 {
            let completed = &matches[0];
            self.input = format!("{prefix}{completed}");
            self.input_cursor_end();
            self.cursor_on = true;
            self.clear_notice();
            return;
        }
        // Longest common prefix of all matches.
        let common = longest_common_prefix(&matches);
        if common.len() > partial.len() {
            self.input = format!("{prefix}{common}");
            self.input_cursor_end();
            self.cursor_on = true;
        }
        let preview: Vec<_> = matches.iter().take(8).cloned().collect();
        self.set_notice(format!(
            "{} matches · {}",
            matches.len(),
            preview.join("  ")
        ));
    }
}
