//! Prompt submission: idle submit, busy follow-up / steer.

use crate::state::{PendingImage, RunOutcome};

use super::helpers::{expand_at_files, is_ui_slash};

impl super::App {
    pub(crate) fn submit_prompt(&mut self) -> RunOutcome {
        // Keep multi-line body; only trim ends.
        self.sync_pending_chips();
        if self.has_loading_images() {
            self.set_notice("still pasting image… · wait a moment");
            return RunOutcome::Noop;
        }
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        if text == "/quit" || text == "/exit" {
            self.pending_images.clear();
            self.pending_texts.clear();
            self.committed_images.clear();
            self.input.clear();
            return RunOutcome::Quit;
        }
        if text == "/help" {
            self.set_notice(
                "/session /resume /new /model · Ctrl+V image · paste path/[文本.txt] · Ctrl+J nl",
            );
            return RunOutcome::Noop;
        }
        if text == "/clear" {
            self.messages.clear();
            self.pending_images.clear();
            self.pending_texts.clear();
            self.committed_images.clear();
            self.input.clear();
            self.chat_scroll = 0;
            self.follow_bottom = true;
            self.set_notice("chat cleared");
            return RunOutcome::Noop;
        }
        // Bare /model → float picker (secondary menu in the float UI).
        if text == "/model" || text == "/model " {
            self.open_model_picker();
            return RunOutcome::Noop;
        }
        // UI slash commands — handled by one-cli without adding a chat turn.
        // Also skip ↑ prompt history: recalling `/model` / `/session` is noise.
        if is_ui_slash(&text) {
            self.input.clear();
            return RunOutcome::Prompt(text);
        }

        // Stage image paths for the agent (input will be cleared).
        let img_order = one_core::image::image_token_ids_in(&text);
        let mut img_by_id: std::collections::HashMap<u32, PendingImage> = self
            .pending_images
            .drain(..)
            .map(|img| (img.id, img))
            .collect();
        self.committed_images = img_order
            .into_iter()
            .filter_map(|id| img_by_id.remove(&id))
            .filter(|img| !img.loading && !img.path.as_os_str().is_empty())
            .map(|img| (img.mime_type, img.path.display().to_string()))
            .collect();

        // Expand `[文本.txt]` bodies for the model; strip image tokens (sent as blocks).
        let text_bodies: std::collections::HashMap<u32, String> = self
            .pending_texts
            .drain(..)
            .map(|t| (t.id, t.body))
            .collect();
        let plain = one_core::image::materialize_prompt_text(&text, &text_bodies);
        let expanded = if plain.contains('@') {
            expand_at_files(&plain)
        } else {
            plain
        };

        // Transcript keeps compact chips (`[图片.img]` / `[文本.txt]`), not the full paste.
        self.push_prompt_history(&text);
        self.input.clear();
        self.push_user(&text);
        RunOutcome::Prompt(expanded)
    }

    pub(crate) fn submit_followup(&mut self) -> RunOutcome {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        self.push_user(&text);
        self.followup_pending = Some(text.clone());
        RunOutcome::FollowUp(text)
    }

    pub(crate) fn submit_steer(&mut self) -> RunOutcome {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return RunOutcome::Noop;
        }
        self.push_user(&text);
        self.steer_pending = Some(text.clone());
        RunOutcome::Steer(text)
    }
}
