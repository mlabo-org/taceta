//! One interface over the coding CLIs Taceta drives as child processes.
//!
//! Each CLI keeps its own session, tools, and context management. Taceta
//! supplies the prompt, shows progress, answers approval requests, and stops
//! the run. A driver translates one CLI's stdio protocol into the types here;
//! nothing outside a driver sees that protocol.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod process;

use crate::domain::Attachment;
use std::{future::Future, path::PathBuf, pin::Pin};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

pub type CliFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Who decides whether an action may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Every action that changes files or runs a command is sent to Taceta.
    AskEveryTime,
    /// The CLI's own permission settings decide; Taceta is asked only when
    /// those settings ask.
    FollowCli,
    /// Nothing may run: the turn is conversation only. The CLI is started
    /// without tools where it can be, and every tool call is refused.
    NoTools,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    /// The CLI session to continue, or `None` to start a new one.
    pub session_id: Option<String>,
    pub workspace: PathBuf,
    /// A model id from [`CliAgent::models`], or `None` for the CLI's default.
    pub model: Option<String>,
    /// A reasoning effort offered by the chosen model.
    pub effort: Option<String>,
    pub prompt: String,
    /// Files that go with the prompt. Empty for a CLI that does not
    /// [`CliAgent::accepts_attachments`].
    pub attachments: Vec<Attachment>,
    pub approval: ApprovalPolicy,
    /// Includes time spent waiting for an approval.
    pub max_duration_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

/// One action the CLI took, updated in place as it progresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolItem {
    pub id: String,
    pub title: String,
    /// What was run or returned, as plain text.
    pub detail: String,
    /// A unified-style diff when the action edited a file.
    pub diff: Option<String>,
    pub status: ToolStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Taceta's own identifier. The CLI's request id stays inside the driver.
    pub id: Uuid,
    pub title: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// One question the CLI puts to the user before it goes on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The CLI's name for the question; the answer is sent back under it.
    pub id: String,
    pub header: String,
    pub question: String,
    /// Suggested answers. Empty when the answer is free text.
    pub options: Vec<QuestionOption>,
    /// The answer must not be shown while it is typed.
    pub is_secret: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The session that this run reads and writes. Sent once, before output.
    SessionReady { session_id: String },
    MessageDelta(String),
    ThinkingDelta(String),
    Tool(ToolItem),
    Approval(ApprovalRequest),
    /// Questions to answer together. `id` is Taceta's own identifier.
    Questions { id: Uuid, questions: Vec<Question> },
    /// The approval or the questions were answered or withdrawn and need no
    /// decision any more.
    ApprovalResolved { id: Uuid },
    Usage(Usage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Approval { id: Uuid, approved: bool },
    /// One answer per question of an [`Event::Questions`], by question id.
    Answers { id: Uuid, answers: Vec<(String, String)> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Interrupted,
    LimitReached,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    pub session_id: Option<String>,
    pub status: RunStatus,
    pub error: Option<String>,
}

impl RunOutcome {
    pub fn failed(session_id: Option<String>, error: impl Into<String>) -> Self {
        Self { session_id, status: RunStatus::Failed, error: Some(error.into()) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub context_window: Option<u64>,
    /// Reasoning efforts the model accepts, weakest first. Empty when the
    /// CLI offers no such control for it.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    /// The model reads images.
    pub images: bool,
}

/// Whether the CLI can be used right now, and what the user must do if not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub executable: PathBuf,
    pub version: Option<String>,
    /// `None` when the CLI offers no way to ask without starting a session.
    pub signed_in: Option<bool>,
}

pub trait CliAgent: Send + Sync {
    /// The name shown for this CLI, e.g. in messages about its state.
    fn name(&self) -> &'static str;

    /// Locates the CLI and asks whether it is signed in. Sign-in itself is
    /// done by the user in the CLI; Taceta never handles its credentials.
    fn readiness(&self) -> CliFuture<Result<Readiness, String>>;

    /// The models to offer now. Answers as soon as the CLI can name any.
    fn models(&self) -> CliFuture<Result<Vec<ModelOption>, String>>;

    /// A newer list the CLI announced after [`CliAgent::models`] answered,
    /// for a CLI that first answers from a list it had stored. `None` when
    /// there is no such list or it did not arrive.
    fn refreshed_models(&self) -> CliFuture<Option<Vec<ModelOption>>> {
        Box::pin(async { None })
    }

    /// Whether a prompt may carry [`RunRequest::attachments`]. Images also
    /// need a model whose [`ModelOption::images`] is set.
    fn accepts_attachments(&self) -> bool {
        true
    }

    /// Runs one prompt to its end. Dropping the returned future, or setting
    /// `cancel`, stops the CLI and everything it started.
    fn run(
        &self,
        request: RunRequest,
        events: mpsc::UnboundedSender<Event>,
        controls: mpsc::UnboundedReceiver<Control>,
        cancel: watch::Receiver<bool>,
    ) -> CliFuture<RunOutcome>;
}

/// An attached text file as every CLI is given it, as its own text block.
pub(crate) fn attached_text(attachment: &Attachment, content: &str) -> String {
    format!("Attached document: {}\n\n{content}", attachment.name)
}

/// The image types every CLI here accepts.
pub(crate) fn accepted_image_type(media_type: &str) -> bool {
    matches!(media_type, "image/png" | "image/jpeg" | "image/webp" | "image/gif")
}
