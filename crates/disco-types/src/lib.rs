//! Core domain types shared by every Disco crate. No IO lives here.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// How an agent is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Remote agent speaking Google A2A over HTTP(S).
    A2a,
    /// Local program speaking ACP over stdio, bridged to A2A.
    Acp,
    /// OpenAI-compatible chat endpoint, wrapped as a one-shot task.
    Openai,
}

/// Lifecycle of one delegated unit of work (mirrors A2A TaskState).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Canceled,
    Failed,
    Rejected,
}

impl TaskPhase {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Canceled | Self::Failed | Self::Rejected
        )
    }
}

/// Transport-neutral event emitted while an agent works on a prompt.
/// Both the A2A client and the ACP bridge normalise into this, so the
/// router and the Discord layer never care where an agent lives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A piece of assistant text (streaming).
    TextChunk { text: String },
    /// Assistant reasoning, shown collapsed or dropped.
    Thought { text: String },
    /// A tool call started or changed state.
    Tool { id: String, title: String, status: String },
    /// The agent asks permission; resolved through [`PermissionDecision`].
    PermissionAsked { request_id: String, title: String, options: Vec<PermissionChoice> },
    /// Task phase changed.
    Phase { phase: TaskPhase },
    /// Final event. `text` is the full concatenated answer when known.
    Done { phase: TaskPhase, stop_reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChoice {
    pub id: String,
    pub label: String,
    pub allow: bool,
    /// "Always allow": the agent stops asking for this kind of action.
    #[serde(default)]
    pub always: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecision {
    Select { option_id: String },
    Cancel,
}
