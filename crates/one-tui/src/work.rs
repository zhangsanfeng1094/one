//! UI-only projection of work from independent runtime registries.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Agent,
    Bash,
    /// Reserved for a future formal task source.
    Task,
}

impl WorkKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "AGENT",
            Self::Bash => "BASH",
            Self::Task => "TASK",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    NeedsAttention,
    Failed,
    Running,
    Queued,
    Completed,
    Stopped,
}

impl WorkState {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Queued)
    }

    pub fn is_attention(self) -> bool {
        matches!(self, Self::NeedsAttention | Self::Failed)
    }

    pub fn section(self) -> &'static str {
        match self {
            Self::NeedsAttention | Self::Failed => "ATTENTION",
            Self::Running | Self::Queued => "RUNNING",
            Self::Completed | Self::Stopped => "RECENT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub id: String,
    pub kind: WorkKind,
    pub state: WorkState,
    pub title: String,
    pub activity: String,
    pub elapsed_ms: u64,
    pub progress: String,
    /// Monotonic registry sequence when available; used for recent retention.
    pub order: u64,
}

impl WorkItem {
    pub fn stable_id(&self) -> String {
        format!("{}:{}", self.kind.label(), self.id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkSummary {
    pub running: usize,
    pub queued: usize,
    pub failed: usize,
    pub attention: usize,
    pub single_live_title: Option<String>,
}

impl WorkSummary {
    pub fn from_items(items: &[WorkItem]) -> Self {
        let mut summary = Self::default();
        for item in items {
            match item.state {
                WorkState::Running => summary.running += 1,
                WorkState::Queued => summary.queued += 1,
                WorkState::Failed => summary.failed += 1,
                WorkState::NeedsAttention => summary.attention += 1,
                _ => {}
            }
        }
        if summary.running + summary.queued == 1 {
            summary.single_live_title = items
                .iter()
                .find(|item| item.state.is_live())
                .map(|item| item.title.clone());
        }
        summary
    }

    pub fn visible(&self) -> bool {
        self.running + self.queued + self.failed + self.attention > 0
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.running > 0 {
            parts.push(format!("{} running", self.running));
        }
        if self.queued > 0 {
            parts.push(format!("{} queued", self.queued));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.attention > 0 {
            parts.push(format!("{} need attention", self.attention));
        }
        if let Some(title) = &self.single_live_title {
            parts.push(title.clone());
        }
        parts.join(" · ")
    }
}

/// Keep every actionable item, followed by at most five recent terminal items.
pub fn overview_items(mut items: Vec<WorkItem>) -> Vec<WorkItem> {
    items.sort_by(|a, b| {
        let rank = |state: WorkState| match state {
            WorkState::NeedsAttention => 0,
            WorkState::Failed => 1,
            WorkState::Running => 2,
            WorkState::Queued => 3,
            WorkState::Completed | WorkState::Stopped => 4,
        };
        rank(a.state)
            .cmp(&rank(b.state))
            .then_with(|| b.order.cmp(&a.order))
            .then_with(|| a.stable_id().cmp(&b.stable_id()))
    });
    let mut recent = 0;
    items.retain(|item| {
        if matches!(item.state, WorkState::Completed | WorkState::Stopped) {
            recent += 1;
            recent <= 5
        } else {
            true
        }
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: WorkKind, state: WorkState, order: u64) -> WorkItem {
        WorkItem {
            id: id.into(),
            kind,
            state,
            title: id.into(),
            activity: String::new(),
            elapsed_ms: 1000,
            progress: String::new(),
            order,
        }
    }

    #[test]
    fn summary_and_retention_are_attention_first() {
        let mut items = vec![
            item("agent", WorkKind::Agent, WorkState::Running, 1),
            item("bash", WorkKind::Bash, WorkState::Running, 2),
            item("failed", WorkKind::Bash, WorkState::Failed, 3),
        ];
        for n in 0..9 {
            items.push(item(
                &format!("done-{n}"),
                WorkKind::Agent,
                WorkState::Completed,
                n,
            ));
        }
        let summary = WorkSummary::from_items(&items);
        assert_eq!((summary.running, summary.failed), (2, 1));
        let overview = overview_items(items);
        assert_eq!(overview[0].id, "failed");
        assert!(overview.iter().any(|i| i.kind == WorkKind::Agent));
        assert!(overview.iter().any(|i| i.kind == WorkKind::Bash));
        assert_eq!(
            overview
                .iter()
                .filter(|i| i.state == WorkState::Completed)
                .count(),
            5
        );
    }

    #[test]
    fn completed_does_not_hold_dock() {
        assert!(!WorkSummary::from_items(&[]).visible());
        assert!(!WorkSummary::from_items(&[item(
            "done",
            WorkKind::Agent,
            WorkState::Completed,
            1
        )])
        .visible());
        let summary =
            WorkSummary::from_items(&[item("repair", WorkKind::Agent, WorkState::Running, 1)]);
        assert!(summary.visible());
        assert!(summary.label().contains("1 running · repair"));
    }
}
