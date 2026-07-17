//! Agent operating mode (Build/Act vs Plan).

/// Agent operating mode (Build/Act vs Plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// Full coding tools — implement changes.
    #[default]
    Act,
    /// Explore + write plan only; no shell / app edits.
    Plan,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Act => "act",
            AgentMode::Plan => "plan",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentMode::Act => "Build",
            AgentMode::Plan => "Plan",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "act" | "build" | "agent" => Some(AgentMode::Act),
            "plan" => Some(AgentMode::Plan),
            _ => None,
        }
    }
}
