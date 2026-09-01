use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentPayload {
    Text(String),
    Image { media_type: String, base64: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub payload: AttachmentPayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub role: Role,
    pub content: String,
    pub thinking: String,
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub citations: Vec<String>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            role,
            content: content.into(),
            thinking: String::new(),
            attachments: Vec::new(),
            citations: Vec::new(),
        }
    }
    pub fn new_user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn new_assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
    pub fn new_system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThinkingLevel {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThinkingMode {
    /// Let the selected model/backend decide; no explicit thinking control is sent.
    Default,
    Off,
    On,
    Level(ThinkingLevel),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingCapability {
    None,
    Toggle,
    Levels,
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub name: String,
    pub size: u64,
    pub thinking: ThinkingCapability,
    pub vision: bool,
    #[serde(default)]
    pub tools: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPullRequest {
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelManagerEvent {
    Started {
        model: String,
    },
    Progress {
        status: String,
        completed: Option<u64>,
        total: Option<u64>,
    },
    Completed {
        model: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub thinking: ThinkingMode,
    pub context_length: u32,
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    #[serde(default)]
    pub web_search_provider: Option<String>,
    #[serde(default = "default_max_search_results")]
    pub max_search_results: u8,
    #[serde(default)]
    pub fetch_search_pages: bool,
}

fn default_max_search_results() -> u8 {
    5
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationStats {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_duration_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerationEvent {
    ThinkingDelta(String),
    ContentDelta(String),
    ToolCall(serde_json::Value),
    SearchProgress(String),
    Citation(String),
    Completed(GenerationStats),
}

pub use Role::*;
