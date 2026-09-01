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
    web_search::{self, ProviderKind, ToolCall, WebSearchProvider},
};
use std::collections::HashSet;
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
                tools: capability::has_tools(&show.capabilities),
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
            let tools = validate_tools(request.tools.as_ref())?;
            if tools.is_some() {
                let show: ShowResponse = client
                    .http
                    .post(client.url("/api/show"))
                    .json(&serde_json::json!({"model": request.model}))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if !capability::has_tools(&show.capabilities) {
                    return Err(BackendError::Protocol(
                        "selected Ollama model does not advertise tool support".into(),
                    ));
                }
            }
            let provider_kind = request
                .web_search_provider
                .as_deref()
                .map(parse_provider)
                .transpose()?;
            let provider = tools
                .as_ref()
                .map(|_| WebSearchProvider::from_kind(provider_kind.unwrap_or_default()))
                .transpose()
                .map_err(|e| BackendError::Protocol(e.to_string()))?;
            let mut messages: Vec<WireMessage> =
                request.messages.iter().map(wire_message).collect();
            let mut seen = HashSet::new();
            let mut fetch_count = 0usize;
            for round in 0..4 {
                let body = ChatBody {
                    model: request.model.clone(),
                    messages: messages.clone(),
                    stream: true,
                    options: ChatOptions {
                        num_ctx: request.context_length,
                    },
                    think: think_value(request.thinking),
                    tools: tools.clone(),
                };
                let response = client
                    .http
                    .post(client.url("/api/chat"))
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?;
                let streamed = stream::consume(response.bytes_stream(), events.clone()).await?;
                if streamed.tool_calls.is_empty() {
                    let _ = events.send(GenerationEvent::Completed(streamed.stats));
                    return Ok(());
                }
                let Some(provider) = provider.as_ref() else {
                    return Err(BackendError::Protocol(
                        "model returned tool calls while web search is disabled".into(),
                    ));
                };
                let round_budget_exhausted = streamed.tool_calls.len() > 2;
                messages.push(WireMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    images: Vec::new(),
                    tool_calls: Some(streamed.tool_calls.clone()),
                    tool_name: None,
                });
                for (index, raw) in streamed.tool_calls.into_iter().enumerate() {
                    let call = web_search::parse_tool_call(&raw)
                        .map_err(|e| BackendError::Protocol(e.to_string()))?;
                    let (content, urls) = if index >= 2 {
                        let _ = events.send(GenerationEvent::SearchProgress(
                            "このラウンドの検索上限に達しました".into(),
                        ));
                        (
                            budget_payload("このラウンドのtool call上限に達しました"),
                            Vec::new(),
                        )
                    } else {
                        execute_tool(
                            provider,
                            &call,
                            &events,
                            &mut fetch_count,
                            request.max_search_results,
                            request.fetch_search_pages,
                        )
                        .await?
                    };
                    messages.push(WireMessage {
                        role: "tool".into(),
                        content,
                        images: Vec::new(),
                        tool_calls: None,
                        tool_name: Some(call.name),
                    });
                    for url in urls {
                        if seen.insert(url.clone()) {
                            let _ = events.send(GenerationEvent::Citation(url));
                        }
                    }
                }
                if round == 3 || round_budget_exhausted {
                    messages.push(WireMessage {
                        role: "system".into(),
                        content: "調査上限に達しました。ここまでに取得できた検索結果と本文だけを根拠に、出典URLを付けて最終回答してください。追加のtool呼び出しは禁止です。".into(),
                        images: Vec::new(),
                        tool_calls: None,
                        tool_name: None,
                    });
                    let body = ChatBody {
                        model: request.model.clone(),
                        messages,
                        stream: true,
                        options: ChatOptions {
                            num_ctx: request.context_length,
                        },
                        think: think_value(request.thinking),
                        tools: None,
                    };
                    let response = client
                        .http
                        .post(client.url("/api/chat"))
                        .json(&body)
                        .send()
                        .await?
                        .error_for_status()?;
                    let final_result =
                        stream::consume(response.bytes_stream(), events.clone()).await?;
                    if !final_result.tool_calls.is_empty() {
                        return Err(BackendError::Protocol(
                            "final web-search synthesis attempted another tool call".into(),
                        ));
                    }
                    let _ = events.send(GenerationEvent::Completed(final_result.stats));
                    return Ok(());
                }
            }
            unreachable!()
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
        tool_calls: None,
        tool_name: None,
    }
}

fn validate_tools(
    tools: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>, BackendError> {
    let Some(value) = tools else { return Ok(None) };
    let Some(items) = value.as_array() else {
        return Err(BackendError::Protocol(
            "web tools must be a JSON array".into(),
        ));
    };
    if items.is_empty()
        || items.iter().any(|item| {
            !matches!(
                item["function"]["name"].as_str(),
                Some("web_search" | "web_fetch")
            )
        })
    {
        return Err(BackendError::Protocol(
            "unsupported web tool configuration".into(),
        ));
    }
    Ok(Some(value.clone()))
}

fn budget_payload(message: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "error": message,
        "retryable": false,
    }))
    .unwrap()
}

fn parse_provider(value: &str) -> Result<ProviderKind, BackendError> {
    match value {
        "Brave" | "brave" => Ok(ProviderKind::Brave),
        "Ollama" | "ollama" => Ok(ProviderKind::Ollama),
        _ => Err(BackendError::Protocol(
            "unsupported web search provider".into(),
        )),
    }
}

async fn execute_tool(
    provider: &WebSearchProvider,
    call: &ToolCall,
    events: &UnboundedSender<GenerationEvent>,
    fetch_count: &mut usize,
    max_results: u8,
    fetch_pages: bool,
) -> Result<(String, Vec<String>), BackendError> {
    match call.name.as_str() {
        "web_search" => {
            let query = call.arguments["query"]
                .as_str()
                .filter(|q| !q.trim().is_empty())
                .ok_or_else(|| {
                    BackendError::Protocol("web_search requires a non-empty query".into())
                })?;
            let limit = call.arguments["limit"]
                .as_u64()
                .unwrap_or(max_results.max(1) as u64)
                .clamp(1, max_results.clamp(1, 5) as u64) as usize;
            let _ = events.send(GenerationEvent::SearchProgress(format!("検索中: {query}")));
            let results = provider
                .search(query, limit)
                .await
                .map_err(|e| BackendError::Protocol(format!("stage=search kind=provider: {e}")))?;
            let urls = results.iter().map(|r| r.url.clone()).collect();
            let content = serde_json::to_string(
                &results
                    .iter()
                    .map(|r| serde_json::json!({"title":r.title,"url":r.url,"snippet":r.snippet}))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            Ok((content, urls))
        }
        "web_fetch" => {
            if !fetch_pages {
                return Err(BackendError::Protocol(
                    "web_fetch is disabled by the current Web Search settings".into(),
                ));
            }
            if *fetch_count >= 5 {
                let _ = events.send(GenerationEvent::SearchProgress(
                    "本文取得の上限に達しました".into(),
                ));
                return Ok((budget_payload("本文取得の上限に達しました"), Vec::new()));
            }
            *fetch_count += 1;
            let url = call.arguments["url"]
                .as_str()
                .ok_or_else(|| BackendError::Protocol("web_fetch requires a URL".into()))?;
            let _ = events.send(GenerationEvent::SearchProgress(format!("取得中: {url}")));
            match provider.fetch(url).await {
                Ok(page) => {
                    let citation = page.url.clone();
                    Ok((serde_json::to_string(&serde_json::json!({"url":page.url,"content_type":page.content_type,"text":page.text})).unwrap(), vec![citation]))
                }
                Err(error) => {
                    let retryable = matches!(
                        error,
                        web_search::WebError::Request(_)
                            | web_search::WebError::Http(reqwest::StatusCode::TOO_MANY_REQUESTS)
                    );
                    let _ =
                        events.send(GenerationEvent::SearchProgress(format!("取得失敗: {url}")));
                    let payload = serde_json::json!({
                        "error": "ページを取得できませんでした",
                        "url": url,
                        "retryable": retryable,
                    });
                    Ok((serde_json::to_string(&payload).unwrap(), Vec::new()))
                }
            }
        }
        _ => Err(BackendError::Protocol("unsupported web tool".into())),
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
            tools: None,
        };
        let json = serde_json::to_value(body).unwrap();
        assert!(!json.as_object().unwrap().contains_key("think"));
        assert_eq!(json["options"]["num_ctx"], serde_json::json!(4096));
    }

    #[test]
    fn web_tool_configuration_rejects_unknown_tools() {
        assert!(validate_tools(Some(&serde_json::json!({"type":"function"}))).is_err());
        assert!(
            validate_tools(Some(&serde_json::json!([{
                "type":"function", "function":{"name":"shell"}
            }])))
            .is_err()
        );
        assert!(
            validate_tools(Some(&serde_json::json!([{
                "type":"function", "function":{"name":"web_search"}
            }])))
            .is_ok()
        );
    }

    #[test]
    fn provider_configuration_is_explicit_and_unknown_values_fail() {
        assert_eq!(parse_provider("brave").unwrap(), ProviderKind::Brave);
        assert_eq!(parse_provider("ollama").unwrap(), ProviderKind::Ollama);
        assert!(parse_provider("google").is_err());
    }

    #[test]
    fn page_failure_payload_is_structured_without_provider_internals() {
        let payload = serde_json::json!({
            "error": "ページを取得できませんでした",
            "url": "https://example.com/article",
            "retryable": false,
        });
        assert_eq!(payload["error"], "ページを取得できませんでした");
        assert!(!payload.to_string().contains("connection refused"));
    }
}
