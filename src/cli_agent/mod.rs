//! One interface over the coding CLIs Taceta drives as child processes.
//!
//! Each CLI keeps its own session, tools, and context management. Taceta
//! supplies the prompt, shows progress, answers approval requests, and stops
//! the run. A driver translates one CLI's stdio protocol into the types here;
//! nothing outside a driver sees that protocol.

pub mod process;

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
    /// The request was answered or withdrawn and needs no decision any more.
    ApprovalResolved { id: Uuid },
    Usage(Usage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Approval { id: Uuid, approved: bool },
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

    fn models(&self) -> CliFuture<Result<Vec<ModelOption>, String>>;

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
