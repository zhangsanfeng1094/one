//! Shared human-in-the-loop channel for permission UI and `ask_user`.
//!
//! The agent/tool task blocks on a oneshot; the TUI loop polls and responds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_tools::AskUserHandler;
use one_tui::{SelectOption, SelectPrompt, SelectResult};
use serde_json::{json, Value};
use tokio::sync::oneshot;

static SEQ: AtomicU64 = AtomicU64::new(1);

/// One question presented via the select UI (ask_user).
#[derive(Debug, Clone)]
pub struct HitlQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<SelectOption>,
    pub multi_select: bool,
}

/// Pending HITL request visible to the TUI.
#[derive(Debug, Clone)]
pub struct HitlSelectRequest {
    pub id: u64,
    pub prompt: SelectPrompt,
}

struct Pending {
    request: HitlSelectRequest,
    tx: oneshot::Sender<SelectResult>,
}

/// Interactive select channel used by `AskUserTool`.
#[derive(Clone)]
pub struct HitlChannel {
    inner: Arc<HitlInner>,
}

struct HitlInner {
    /// When false, ask_user fails closed immediately (print / rpc).
    interactive: bool,
    pending: Mutex<Option<Pending>>,
}

impl HitlChannel {
    pub fn new(interactive: bool) -> Self {
        Self {
            inner: Arc::new(HitlInner {
                interactive,
                pending: Mutex::new(None),
            }),
        }
    }

    pub fn is_interactive(&self) -> bool {
        self.inner.interactive
    }

    pub fn poll_request(&self) -> Option<HitlSelectRequest> {
        self.inner
            .pending
            .lock()
            .expect("hitl lock")
            .as_ref()
            .map(|p| p.request.clone())
    }

    pub fn respond(&self, result: SelectResult) -> bool {
        let mut g = self.inner.pending.lock().expect("hitl lock");
        if let Some(pending) = g.take() {
            let _ = pending.tx.send(result);
            true
        } else {
            false
        }
    }

    pub fn cancel_pending(&self) {
        if let Some(pending) = self.inner.pending.lock().expect("hitl lock").take() {
            let _ = pending.tx.send(SelectResult::Cancelled);
        }
    }

    /// Block until the user answers one select prompt.
    pub async fn prompt_select(&self, mut prompt: SelectPrompt) -> Result<SelectResult, String> {
        if !self.inner.interactive {
            return Err(
                "ask_user requires interactive mode (TUI). Re-run without -p / --mode print."
                    .into(),
            );
        }
        // Ensure free-text Other is available for clarifying questions.
        if !prompt.allow_other {
            prompt.allow_other = true;
            if prompt.other_label.is_empty() {
                prompt.other_label = "Other (type free text)".into();
            }
        }
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.inner.pending.lock().expect("hitl lock");
            if g.is_some() {
                return Err("another human-in-the-loop prompt is already pending".into());
            }
            *g = Some(Pending {
                request: HitlSelectRequest { id, prompt },
                tx,
            });
        }
        rx.await
            .map_err(|_| "ask_user cancelled (session aborted)".to_string())
    }

    /// Ask one structured question (single or multi select).
    pub async fn ask_one(&self, q: HitlQuestion) -> Result<(Vec<String>, Option<String>), String> {
        let title = if q.header.is_empty() {
            "Question".into()
        } else {
            q.header.clone()
        };
        let mut prompt = if q.multi_select {
            SelectPrompt::multi(title, q.question.clone(), q.options)
        } else {
            SelectPrompt::single(title, q.question.clone(), q.options)
        };
        prompt.allow_other = true;
        prompt.other_label = "Other (type free text)".into();
        prompt.footer_hint = if q.multi_select {
            "↑↓:move  Space:toggle  Tab:other  Enter:confirm  Esc:cancel".into()
        } else {
            "↑↓/1-n:select  Tab:other  Enter:confirm  Esc:cancel".into()
        };

        match self.prompt_select(prompt).await? {
            SelectResult::Cancelled => Err("user cancelled ask_user".into()),
            SelectResult::Confirmed { ids, other } => {
                // Map ids back to labels when ids are labels (we store label as id for ask_user).
                Ok((ids, other))
            }
        }
    }
}

/// Build select options from ask_user JSON options (label + description).
pub fn options_from_json(opts: &[serde_json::Value]) -> Result<Vec<SelectOption>, String> {
    if opts.len() < 2 || opts.len() > 4 {
        return Err(format!(
            "each question needs 2-4 options, got {}",
            opts.len()
        ));
    }
    let mut out = Vec::with_capacity(opts.len());
    for (i, o) in opts.iter().enumerate() {
        let label = o
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("options[{i}].label required"))?
            .to_string();
        let description = o
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Use label as id so answers map cleanly back to Claude-style labels.
        out.push(SelectOption::new(label.clone(), label, description));
    }
    Ok(out)
}

pub fn parse_questions(args: &serde_json::Value) -> Result<Vec<HitlQuestion>, String> {
    let questions = args
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "questions array required".to_string())?;
    if questions.is_empty() || questions.len() > 4 {
        return Err(format!(
            "ask_user supports 1-4 questions, got {}",
            questions.len()
        ));
    }
    let mut out = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        let question = q
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("questions[{i}].question required"))?
            .to_string();
        let header = q
            .get("header")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let multi = q
            .get("multi_select")
            .or_else(|| q.get("multiSelect"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let opts = q
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("questions[{i}].options required"))?;
        let options = options_from_json(opts)?;
        out.push(HitlQuestion {
            question,
            header,
            options,
            multi_select: multi,
        });
    }
    Ok(out)
}

/// Bridge from `AskUserTool` → TUI select prompts (one question at a time).
pub struct InteractiveAskUser {
    channel: HitlChannel,
}

impl InteractiveAskUser {
    pub fn new(channel: HitlChannel) -> Self {
        Self { channel }
    }
}

#[async_trait]
impl AskUserHandler for InteractiveAskUser {
    async fn ask(&self, questions: Vec<Value>) -> std::result::Result<Value, String> {
        let parsed = parse_questions(&json!({ "questions": questions }))?;
        let mut answers = serde_json::Map::new();
        for q in parsed {
            let qtext = q.question.clone();
            let multi = q.multi_select;
            let (ids, other) = self.channel.ask_one(q).await?;
            let value = if let Some(text) = other.filter(|s| !s.is_empty()) {
                // Free-text takes precedence when provided alone; if multi also checked, combine.
                if multi && !ids.is_empty() {
                    let mut labels = ids;
                    labels.push(text);
                    Value::Array(labels.into_iter().map(Value::String).collect())
                } else {
                    Value::String(text)
                }
            } else if multi {
                Value::Array(ids.into_iter().map(Value::String).collect())
            } else {
                Value::String(ids.into_iter().next().unwrap_or_default())
            };
            answers.insert(qtext, value);
        }
        Ok(Value::Object(answers))
    }
}
