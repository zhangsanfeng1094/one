//! HITL prompts: tool approval and ask_user / model select docks.

use crate::state::{ApprovalAnswer, ApprovalPrompt, RunOutcome, SelectKind};

impl super::App {
    /// Show a tool-approval modal (called from CLI while agent is busy).
    pub fn set_approval_prompt(&mut self, prompt: ApprovalPrompt) {
        // Don't wipe an answer waiting for CLI drain, and don't re-open the same id.
        if self.approval_answer.is_some() {
            return;
        }
        if self.approval.as_ref().map(|p| p.id) == Some(prompt.id) {
            return;
        }
        let select = crate::select::SelectPrompt::permission_with_prefix(
            &prompt.tool,
            &prompt.summary,
            &prompt.reason,
            prompt.suggested_prefix.as_deref(),
        );
        let id = prompt.id;
        self.approval = Some(prompt);
        self.approval_answer = None;
        self.select = Some(select);
        self.select_kind = Some(SelectKind::Approval { id });
        self.select_result = None;
    }

    pub fn clear_approval_prompt(&mut self) {
        self.approval = None;
        if matches!(self.select_kind, Some(SelectKind::Approval { .. })) {
            self.select = None;
            self.select_kind = None;
        }
    }

    pub fn approval_prompt(&self) -> Option<&ApprovalPrompt> {
        self.approval.as_ref()
    }

    /// Active select prompt (permission or ask_user).
    pub fn select_prompt(&self) -> Option<&crate::select::SelectPrompt> {
        self.select.as_ref()
    }

    pub fn select_kind(&self) -> Option<&SelectKind> {
        self.select_kind.as_ref()
    }

    /// Show a generic select prompt (ask_user HITL).
    pub fn set_select_prompt(&mut self, kind: SelectKind, prompt: crate::select::SelectPrompt) {
        // Don't clobber an in-flight dock, or wipe a result waiting for the CLI drain.
        if self.select_result.is_some() {
            return;
        }
        if self.select_kind.as_ref() == Some(&kind) && self.select.is_some() {
            return;
        }
        self.select = Some(prompt);
        self.select_kind = Some(kind);
        self.select_result = None;
    }

    pub fn clear_select_prompt(&mut self) {
        self.select = None;
        self.select_kind = None;
    }

    /// Take a finished select result (ask_user). Approval results go to
    /// [`Self::take_approval_answer`] instead.
    pub fn take_select_result(&mut self) -> Option<(SelectKind, crate::select::SelectResult)> {
        self.select_result.take()
    }

    /// Take the user's answer if any (CLI feeds it into PermissionGate).
    pub fn take_approval_answer(&mut self) -> Option<ApprovalAnswer> {
        self.approval_answer.take()
    }

    /// Apply a finished select; may return a [`RunOutcome`] for CLI to handle.
    pub(crate) fn apply_select_result(
        &mut self,
        result: crate::select::SelectResult,
    ) -> Option<RunOutcome> {
        let kind = match self.select_kind.take() {
            Some(k) => k,
            None => return None,
        };
        self.select = None;
        match kind {
            SelectKind::Approval { .. } => {
                self.approval = None;
                let answer = match result {
                    crate::select::SelectResult::Cancelled => {
                        ApprovalAnswer::Deny { feedback: None }
                    }
                    crate::select::SelectResult::Confirmed { ids, other } => {
                        match ids.first().map(|s| s.as_str()) {
                            Some("always") => ApprovalAnswer::Always,
                            Some("once") => ApprovalAnswer::Once,
                            Some("session") => ApprovalAnswer::Session,
                            Some("prefix") => ApprovalAnswer::Prefix,
                            Some("deny") => ApprovalAnswer::Deny { feedback: other },
                            _ => ApprovalAnswer::Deny { feedback: other },
                        }
                    }
                };
                let notice = match &answer {
                    ApprovalAnswer::Always => "always-approve mode",
                    ApprovalAnswer::Once => "approved once",
                    ApprovalAnswer::Session => "approved for session",
                    ApprovalAnswer::Prefix => "approved command prefix for session",
                    ApprovalAnswer::Deny { feedback } => {
                        if feedback.as_ref().map(|s| !s.is_empty()).unwrap_or(false) {
                            "denied (with feedback)"
                        } else {
                            "denied"
                        }
                    }
                };
                self.set_notice(notice);
                self.approval_answer = Some(answer);
                None
            }
            SelectKind::AskUser { .. } => {
                self.select_result = Some((kind, result));
                None
            }
            SelectKind::ModelProvider => match result {
                crate::select::SelectResult::Cancelled => {
                    self.set_notice("model select cancelled");
                    None
                }
                crate::select::SelectResult::Confirmed { ids, .. } => {
                    let Some(provider) = ids.first() else {
                        return None;
                    };
                    self.open_model_select_for_provider(provider);
                    None
                }
            },
            SelectKind::Model { provider } => match result {
                crate::select::SelectResult::Cancelled => {
                    self.open_model_select();
                    None
                }
                crate::select::SelectResult::Confirmed { ids, .. } => {
                    let Some(model) = ids.first() else {
                        return None;
                    };
                    Some(RunOutcome::SwitchModel {
                        provider,
                        model: Some(model.clone()),
                    })
                }
            },
            SelectKind::EnabledModels => None,
        }
    }

    /// Height of the select dock above the prompt (`0` when closed).
    pub fn select_dock_height(&self) -> u16 {
        use crate::select::SelectPhase;
        let Some(prompt) = self.select.as_ref() else {
            return 0;
        };
        let body = prompt.body.lines().filter(|l| !l.is_empty()).count();
        let body = if body == 0 { 0 } else { body + 1 }; // + blank
        let typing = if matches!(prompt.phase, SelectPhase::Typing { .. }) {
            2
        } else {
            0
        };
        // border(2) + title area absorbed in border + body + options + footer
        let content = body + prompt.option_count().min(8) + typing + 1;
        ((content as u16) + 2).clamp(5, 14)
    }

    /// Open model switcher as docked select (Ctrl+L / `/model`).
    ///
    /// The switcher is cascaded to avoid a long flat `provider:model` list:
    /// first choose a provider, then choose one model from that provider.
    /// Only models in [`Self::enabled_models`] appear when that list is set;
    /// the active model is always included so you can see (and leave) it.
    pub fn open_model_select(&mut self) {
        use crate::select::{SelectOption, SelectPrompt};
        use std::collections::BTreeMap;

        self.close_float();
        let mut by_provider: BTreeMap<String, Vec<&crate::slash::ModelChoice>> = BTreeMap::new();
        for model in self
            .model_catalog
            .iter()
            .filter(|m| self.model_visible_in_switcher(m))
        {
            by_provider
                .entry(model.provider.clone())
                .or_default()
                .push(model);
        }
        if by_provider.is_empty() {
            self.set_notice("no models in catalog");
            return;
        }

        let options: Vec<SelectOption> = by_provider
            .iter()
            .map(|(provider, models)| {
                let current = provider == &self.current_provider;
                let label = if current {
                    format!("{provider}  (current)")
                } else {
                    provider.clone()
                };
                let enabled = models.len();
                let total = self
                    .model_catalog
                    .iter()
                    .filter(|m| m.provider == *provider)
                    .count();
                let desc = if enabled == total {
                    format!("{total} models")
                } else {
                    format!("{enabled}/{total} models shown")
                };
                SelectOption::new(provider.clone(), label, desc)
            })
            .collect();
        let selected = options
            .iter()
            .position(|o| o.id == self.current_provider)
            .unwrap_or(0);
        let body = format!(
            "Choose a provider\nCurrent: {} / {}",
            self.current_provider, self.current_model
        );
        let mut prompt = SelectPrompt::single("Model", body, options);
        prompt.selected = selected;
        prompt.footer_hint = "↑↓ navigate · Enter choose models · Esc cancel".into();
        self.select = Some(prompt);
        self.select_kind = Some(SelectKind::ModelProvider);
        self.select_result = None;
        self.clear_notice();
    }

    /// Second level of the cascaded model switcher: models for one provider.
    pub(crate) fn open_model_select_for_provider(&mut self, provider: &str) {
        use crate::select::{SelectOption, SelectPrompt};

        let options: Vec<SelectOption> = self
            .model_catalog
            .iter()
            .filter(|m| m.provider == provider && self.model_visible_in_switcher(m))
            .map(|m| {
                let label = if m.provider == self.current_provider && m.id == self.current_model {
                    format!("{}  (current)", m.id)
                } else {
                    m.id.clone()
                };
                let desc = if m.name != m.id {
                    m.name.clone()
                } else {
                    String::new()
                };
                SelectOption::new(m.id.clone(), label, desc)
            })
            .collect();
        if options.is_empty() {
            self.open_model_select();
            self.set_notice(format!("no models shown for `{provider}`"));
            return;
        }
        let selected = options
            .iter()
            .position(|o| provider == self.current_provider && o.id == self.current_model)
            .unwrap_or(0);
        let body = if provider == self.current_provider {
            format!(
                "Provider: {provider}\nCurrent model: {}",
                self.current_model
            )
        } else {
            format!("Provider: {provider}\nSwitch from: {}", self.current_model)
        };
        let mut prompt = SelectPrompt::single("Model", body, options);
        prompt.selected = selected;
        prompt.footer_hint = "↑↓ navigate · Enter switch · Esc back".into();
        self.select = Some(prompt);
        self.select_kind = Some(SelectKind::Model {
            provider: provider.to_string(),
        });
        self.select_result = None;
        self.clear_notice();
    }

    /// Whether a catalog model should appear in the Ctrl+L switcher.
    pub(crate) fn model_visible_in_switcher(&self, m: &crate::slash::ModelChoice) -> bool {
        let is_current = m.provider == self.current_provider && m.id == self.current_model;
        if is_current {
            return true;
        }
        match &self.enabled_models {
            None => true,
            Some(specs) if specs.is_empty() => true,
            Some(specs) => {
                let id = format!("{}:{}", m.provider, m.id);
                specs.iter().any(|s| s == &id)
            }
        }
    }
}
