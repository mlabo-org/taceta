use crate::domain::ThinkingMode;
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf, pin::Pin};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

/// Adapters perform exactly one inference. Only the session executor runs tools.
pub trait AgentModel: Send + Sync {
    fn turn(
        &self,
        request: AgentRequest,
        events: UnboundedSender<ModelDelta>,
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurn, String>> + Send>>;
}

#[derive(Clone, Debug)]
pub struct AgentRequest {
    pub model: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<ToolDefinition>,
    pub thinking: ThinkingMode,
    pub context_length: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub role: AgentRole,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<AgentToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
}

impl AgentMessage {
    pub fn text(role: AgentRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentTurn {
    pub content: String,
    pub tool_calls: Vec<AgentToolCall>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum ModelDelta {
    Content(String),
    /// Transient display data. Never journaled or inserted in a later request.
    Thinking(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    AwaitingApproval,
    Completed,
    Interrupted,
    LimitReached,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkItemStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    pub id: String,
    pub description: String,
    pub status: WorkItemStatus,
}

/// Model-maintained facts are separate from immutable user instructions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkState {
    pub tasks: Vec<WorkItem>,
    pub decisions: Vec<String>,
    pub constraints: Vec<String>,
    pub artifacts: Vec<String>,
    pub resume_point: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetainedHistoryRange {
    pub first_seq: u64,
    pub last_seq: u64,
    pub first_message: usize,
    pub last_message: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionSummary {
    pub from_seq: u64,
    pub to_seq: u64,
    pub through_event_id: String,
    pub through_message_index: usize,
    pub window_id: Uuid,
    pub previous_window_id: Option<Uuid>,
    pub source_last_seq: u64,
    pub retained_ranges: Vec<RetainedHistoryRange>,
    pub model: String,
    pub context_length: u32,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: Uuid,
    pub workspace: PathBuf,
    pub status: SessionStatus,
    pub goal: String,
    pub user_instructions: Vec<String>,
    pub work_state: WorkState,
    /// A bounded UI view. The journal retains every original message.
    pub messages: Vec<AgentMessage>,
    pub error: Option<String>,
    pub event_count: u64,
    pub summary: Option<CompactionSummary>,
    pub latest_model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub model: String,
    pub thinking: ThinkingMode,
    pub context_length: u32,
    /// None resumes an unfinished task; Some durably adds a user instruction.
    pub prompt: Option<String>,
    /// Total inference calls, including compaction calls.
    pub max_steps: u32,
    /// Includes inference, tools, and time waiting for approval.
    pub max_duration_secs: u64,
    /// Uses the shared compaction path, then stops without running work tools.
    pub compact_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub tool_call_id: String,
    pub description: String,
    pub details: String,
}

#[derive(Clone, Debug)]
pub struct ApprovalDecision {
    pub id: Uuid,
    pub approved: bool,
}

#[derive(Clone, Debug)]
pub enum AgentEvent {
    Delta(ModelDelta),
    Progress(String),
    ApprovalRequired(ApprovalRequest),
    Snapshot(SessionSnapshot),
}
