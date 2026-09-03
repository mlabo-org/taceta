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
    /// Interrupted output remains visible in the transcript but must never
    /// become input to a later model turn.
    #[serde(default)]
    pub interrupted: bool,
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
            interrupted: false,
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
    /// Context capacity reported by the backend for this exact model.
    #[serde(default)]
    pub context_length: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCandidate {
    pub model: String,
    pub estimated_size: Option<String>,
    pub recommended: bool,
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
    #[serde(default = "default_chatgpt_web_request_limit")]
    pub chatgpt_web_request_limit: u8,
    #[serde(default)]
    pub fetch_search_pages: bool,
    /// The one-shot authorization granted by the Web: ON control for this request.
    #[serde(default)]
    pub web_authorization: Option<WebAuthorization>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAuthorization {
    pub request_id: Uuid,
    pub session_id: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebWorkflow {
    DefaultSearch,
    GoogleSearch,
    PageFetch,
    ChatGptWeb,
}

fn default_max_search_results() -> u8 {
    5
}

pub const MIN_CHATGPT_WEB_REQUEST_LIMIT: u8 = 1;
pub const MAX_CHATGPT_WEB_REQUEST_LIMIT: u8 = 3;
pub const DEFAULT_CHATGPT_WEB_REQUEST_LIMIT: u8 = 1;

fn default_chatgpt_web_request_limit() -> u8 {
    DEFAULT_CHATGPT_WEB_REQUEST_LIMIT
}

pub fn normalize_chatgpt_web_request_limit(value: u8) -> u8 {
    value.clamp(MIN_CHATGPT_WEB_REQUEST_LIMIT, MAX_CHATGPT_WEB_REQUEST_LIMIT)
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
    ReplaceContent(String),
    ExternalContentDelta { delta: String, replace: bool },
    ToolCall(serde_json::Value),
    SearchProgress(String),
    Citation(String),
    Completed(GenerationStats),
}

pub use Role::*;
