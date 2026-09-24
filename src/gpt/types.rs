use crate::domain::{Attachment, ThinkingMode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GptMode {
    Chat,
    Coding,
}

#[derive(Clone, Debug)]
pub enum GptAction {
    Send { text: String, attachments: Vec<Attachment> },
    Continue,
    Compact,
}

#[derive(Clone, Debug)]
pub struct GptRunRequest {
    pub thread_id: Option<String>,
    pub workspace: Option<PathBuf>,
    pub mode: GptMode,
    pub model: String,
    pub thinking: ThinkingMode,
    pub action: GptAction,
    /// Explicit, portable context from a previous Taceta coding session.
    /// Delivered once to an empty Codex thread; retries consult its saved input.
    pub handoff: Option<String>,
    /// Includes time waiting for approval or an answer to a question.
    pub max_duration_secs: u64,
    /// Bounds tool actions emitted by Codex, not hidden model inference calls.
    pub max_tool_actions: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GptRunStatus {
    #[default]
    Idle,
    Running,
    AwaitingApproval,
    Completed,
    Interrupted,
    LimitReached,
    Failed,
    Compacted,
}

#[derive(Clone, Debug)]
pub struct GptRunOutcome {
    pub thread_id: String,
    pub status: GptRunStatus,
    pub error: Option<String>,
}

pub enum GptLoginEvent {
    OpenBrowser(String),
    Progress(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GptToolItem {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub completed: bool,
}

#[derive(Clone, Debug)]
pub struct GptApproval {
    /// A local opaque identifier. Wire request IDs stay inside the adapter.
    pub id: Uuid,
    pub description: String,
    pub details: String,
}

#[derive(Clone, Debug)]
pub struct GptQuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct GptQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<GptQuestionOption>,
    pub is_secret: bool,
}

#[derive(Clone, Debug)]
pub enum GptEvent {
    ThreadReady { thread_id: String },
    HandoffAccepted,
    MessageStarted { id: String },
    ContentDelta { id: String, text: String },
    MessageCompleted { id: String, text: String },
    ThinkingDelta(String),
    Tool(GptToolItem),
    Approval(GptApproval),
    Questions { id: Uuid, questions: Vec<GptQuestion> },
    RequestResolved { id: Uuid },
    Progress(String),
    Diff(String),
    Usage { input_tokens: u64, output_tokens: u64, context_window: Option<u64> },
}

#[derive(Clone, Debug)]
pub enum GptControl {
    /// The UI has durably saved the returned thread ID before starting work.
    ThreadSaved { thread_id: String },
    Approval { id: Uuid, approved: bool },
    Answers { id: Uuid, answers: Vec<(String, String)> },
}
