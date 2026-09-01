use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(super) struct TagsResponse {
    pub models: Vec<TagModel>,
}
#[derive(Debug, Deserialize)]
pub(super) struct TagModel {
    pub name: String,
    #[serde(default)]
    pub size: u64,
}
#[derive(Debug, Deserialize)]
pub(super) struct ShowResponse {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub details: Details,
}
#[derive(Debug, Default, Deserialize)]
pub(super) struct Details {
    #[serde(default)]
    pub family: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ChatBody {
    pub model: String,
    pub messages: Vec<WireMessage>,
    pub stream: bool,
    pub options: ChatOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
}
#[derive(Debug, Serialize)]
pub(super) struct ChatOptions {
    pub num_ctx: u32,
}
#[derive(Debug, Clone, Serialize)]
pub(super) struct WireMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}
#[derive(Debug, Deserialize)]
pub(super) struct ChatChunk {
    #[serde(default)]
    pub message: Option<ChunkMessage>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub total_duration: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}
#[derive(Debug, Deserialize)]
pub(super) struct ChunkMessage {
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
}
