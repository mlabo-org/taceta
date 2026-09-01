use super::{
    api::{ChatBody, ChatOptions, ShowResponse, TagsResponse, WireMessage},
    capability, lifecycle, stream,
};
use crate::{
    backend::{BackendError, BackendFuture, InferenceBackend},
    domain::{
        AttachmentPayload, ChatMessage, ChatRequest, GenerationEvent, ModelDescriptor, Role,
        ThinkingMode,
    },
};
use tokio::sync::mpsc::UnboundedSender;

pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
    async fn models(&self) -> Result<Vec<ModelDescriptor>, BackendError> {
        lifecycle::ensure_ready(&self.http, &self.base_url).await?;
        let tags: TagsResponse = self
            .http
            .get(self.url("/api/tags"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut result = Vec::with_capacity(tags.models.len());
        for model in tags.models {
            let name = model.name;
            let show: ShowResponse = self
                .http
                .post(self.url("/api/show"))
                .json(&serde_json::json!({"model": name}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let thinking = capability::classify(&name, &show.details.family);
            result.push(ModelDescriptor {
                name,
                size: model.size,
                thinking,
                vision: capability::has_vision(&show.capabilities),
            });
        }
        Ok(result)
    }
}

impl InferenceBackend for OllamaClient {
    fn list_models(&self) -> BackendFuture<Vec<ModelDescriptor>> {
        let client = self.clone_for_task();
        Box::pin(async move { client.models().await })
    }
    fn stream_chat(
        &self,
        request: ChatRequest,
        events: UnboundedSender<GenerationEvent>,
    ) -> BackendFuture<()> {
        let client = self.clone_for_task();
        Box::pin(async move {
            lifecycle::ensure_ready(&client.http, &client.base_url).await?;
            let body = ChatBody {
                model: request.model,
                messages: request.messages.iter().map(wire_message).collect(),
                stream: true,
                options: ChatOptions {
                    num_ctx: request.context_length,
                },
                think: think_value(request.thinking),
            };
            let response = client
                .http
                .post(client.url("/api/chat"))
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
            stream::consume(response.bytes_stream(), events).await
        })
    }
}

impl OllamaClient {
    fn clone_for_task(&self) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

fn role(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}
fn wire_message(message: &ChatMessage) -> WireMessage {
    let mut content = message.content.clone();
    let mut images = Vec::new();
    for attachment in &message.attachments {
        match &attachment.payload {
            AttachmentPayload::Text(text) => {
                content.push_str("\n\n--- ");
                content.push_str(&attachment.name);
                content.push_str(" ---\n");
                content.push_str(text);
            }
            AttachmentPayload::Image { base64, .. } => images.push(base64.clone()),
        }
    }
    WireMessage {
        role: role(&message.role).into(),
        content,
        images,
    }
}
fn think_value(mode: ThinkingMode) -> Option<serde_json::Value> {
    match mode {
        ThinkingMode::Default => None,
        ThinkingMode::Off => Some(false.into()),
        ThinkingMode::On => Some(true.into()),
        ThinkingMode::Level(level) => Some(serde_json::Value::String(
            match level {
                crate::domain::ThinkingLevel::Low => "low",
                crate::domain::ThinkingLevel::Medium => "medium",
                crate::domain::ThinkingLevel::High => "high",
            }
            .into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_message_never_includes_thinking_and_routes_attachments() {
        let mut m = ChatMessage::new_user("ask");
        m.thinking = "private trace".into();
        m.attachments.push(crate::domain::Attachment {
            name: "note.txt".into(),
            payload: AttachmentPayload::Text("context".into()),
        });
        m.attachments.push(crate::domain::Attachment {
            name: "pic.png".into(),
            payload: AttachmentPayload::Image {
                media_type: "image/png".into(),
                base64: "abc".into(),
            },
        });
        let w = wire_message(&m);
        assert!(w.content.contains("context") && !w.content.contains("private trace"));
        assert_eq!(w.images, vec!["abc"]);
    }
    #[test]
    fn thinking_modes_map_to_ollama_values() {
        assert_eq!(
            think_value(ThinkingMode::Off),
            Some(serde_json::json!(false))
        );
        assert_eq!(think_value(ThinkingMode::On), Some(serde_json::json!(true)));
        assert_eq!(
            think_value(ThinkingMode::Level(crate::domain::ThinkingLevel::Low)),
            Some(serde_json::json!("low"))
        );
    }

    #[test]
    fn default_thinking_omits_wire_field() {
        let body = ChatBody {
            model: "unknown-model".into(),
            messages: Vec::new(),
            stream: true,
            options: ChatOptions { num_ctx: 4096 },
            think: think_value(ThinkingMode::Default),
        };
        let json = serde_json::to_value(body).unwrap();
        assert!(!json.as_object().unwrap().contains_key("think"));
        assert_eq!(json["options"]["num_ctx"], serde_json::json!(4096));
    }
}
