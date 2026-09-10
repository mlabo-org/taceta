//! Independent Grok OAuth and Responses transport. Conversation state and all
//! tool execution remain owned by Taceta; no Grok CLI process is launched.
mod auth;
mod completions;
mod metadata;
mod responses;

use crate::{
    agent::{
        AgentMessage, AgentModel, AgentRequest, AgentRole, AgentTurn, ModelDelta, ToolDefinition,
    },
    backend::{BackendError, BackendFuture, InferenceBackend},
    domain::{
        AttachmentPayload, ChatRequest, GenerationEvent, GenerationStats, ModelDescriptor, Role,
        ThinkingMode,
    },
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use auth::{AuthEndpoints, AuthManager, CredentialStore, KeychainStore};
use metadata::{ApiKind, ModelInfo, parse_models};
use responses::OutputDelta;

const OAUTH_API_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const CANCELLED: &str = "Grok generation was cancelled.";

pub enum GrokLoginEvent {
    OpenBrowser(String),
    Progress(String),
}

#[derive(Clone)]
pub struct GrokClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    auth: AuthManager,
    api_base: String,
    models: RwLock<HashMap<String, ModelInfo>>,
    session: String,
    turn: AtomicU64,
}

impl Default for GrokClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokClient {
    /// Construction never reads credentials, starts login, or sends a request.
    pub fn new() -> Self {
        Self::configured(
            AuthEndpoints::default(),
            OAUTH_API_BASE.into(),
            Arc::new(KeychainStore),
        )
    }

    fn configured(
        endpoints: AuthEndpoints,
        api_base: String,
        store: Arc<dyn CredentialStore>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120))
            .user_agent(concat!("Taceta/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Taceta's HTTP client configuration is valid");
        Self {
            inner: Arc::new(Inner {
                auth: AuthManager::new(http.clone(), endpoints, store),
                http,
                api_base,
                models: RwLock::new(HashMap::new()),
                session: Uuid::new_v4().to_string(),
                turn: AtomicU64::new(0),
            }),
        }
    }

    pub async fn sign_in(&self, events: UnboundedSender<GrokLoginEvent>) -> Result<(), String> {
        self.inner.auth.sign_in(events).await?;
        self.inner
            .models
            .write()
            .map_err(|_| "Grok model access failed.")?
            .clear();
        Ok(())
    }

    /// Removes only Taceta's own Keychain item; it does not revoke another
    /// application's grant or access a browser's session.
    pub fn sign_out(&self) -> Result<(), String> {
        self.inner.auth.sign_out()?;
        self.inner
            .models
            .write()
            .map_err(|_| "Grok model access failed.")?
            .clear();
        Ok(())
    }

    pub fn is_signed_in(&self) -> Result<bool, String> {
        self.inner.auth.is_signed_in()
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        model: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, String> {
        let token = self.inner.auth.access_token().await?;
        let request_id = Uuid::new_v4().to_string();
        let mut request = self
            .inner
            .http
            .request(method, format!("{}{path}", self.inner.api_base))
            .bearer_auth(token.as_str())
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-authenticateresponse", "authenticate-response")
            .header("x-grok-client-identifier", "taceta")
            .header("x-grok-client-mode", "interactive")
            .header("x-grok-session-id", &self.inner.session)
            .header("x-grok-conv-id", &self.inner.session)
            .header("x-grok-agent-id", &self.inner.session)
            .header("x-grok-req-id", request_id)
            .header(
                "x-grok-turn-idx",
                self.inner.turn.fetch_add(1, Ordering::Relaxed).to_string(),
            );
        // x-grok-client-version denotes Grok CLI's installed build version in
        // the public source. Taceta does not claim that unrelated version. A
        // proxy that requires it returns a visible connection rejection.
        if let Some(model) = model {
            request = request.header("x-grok-model-override", model);
        }
        Ok(request)
    }

    async fn fetch_models(&self) -> Result<Vec<ModelDescriptor>, String> {
        let epoch = self.inner.auth.epoch();
        let response = self
            .request(reqwest::Method::GET, "/models", None)
            .await?
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| "Unable to contact the Grok model service.".to_string())?;
        reject_status(response.status())?;
        let bytes = read_bounded(response, 2 * 1024 * 1024).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Grok returned an invalid model list.")?;
        let models = parse_models(&value)?;
        let descriptors = models
            .iter()
            .map(|model| model.descriptor.clone())
            .collect();
        let mut cache = self
            .inner
            .models
            .write()
            .map_err(|_| "Grok model access failed.")?;
        if epoch != self.inner.auth.epoch() {
            return Err("Grok connection changed while models were loading.".into());
        }
        *cache = models
            .into_iter()
            .map(|model| (model.descriptor.name.clone(), model))
            .collect();
        Ok(descriptors)
    }

    async fn model(&self, name: &str) -> Result<ModelInfo, String> {
        let cached = self
            .inner
            .models
            .read()
            .map_err(|_| "Grok model access failed.")?
            .get(name)
            .cloned();
        if let Some(model) = cached {
            return Ok(model);
        }
        self.fetch_models().await?;
        self.inner.models.read().map_err(|_| "Grok model access failed.")?.get(name).cloned()
            .ok_or_else(|| "The selected model is not in this account's Grok inference catalog. Refresh the model list.".into())
    }

    async fn response(
        &self,
        body: Value,
        model: &str,
        api: ApiKind,
    ) -> Result<reqwest::Response, String> {
        let response = self
            .request(reqwest::Method::POST, api.path(), Some(model))
            .await?
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|_| "Unable to start a response from the Grok service.".to_string())?;
        reject_status(response.status())?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err("Grok did not return the requested inference event stream.".into());
        }
        Ok(response)
    }

    async fn agent_turn(
        &self,
        request: AgentRequest,
        events: UnboundedSender<ModelDelta>,
    ) -> Result<AgentTurn, String> {
        let work = async {
            let model = self.model(&request.model).await?;
            if !request.tools.is_empty() && !model.descriptor.tools {
                return Err("The selected Grok model does not support function tools.".into());
            }
            let input = match model.api {
                ApiKind::Responses => responses::agent_input(&request.messages)?,
                ApiKind::ChatCompletions => completions::agent_input(&request.messages)?,
            };
            let body = payload(
                &request.model,
                &model,
                input,
                &request.tools,
                request.thinking,
                request.context_length,
            )?;
            let response = self.response(body, &request.model, model.api).await?;
            let allowed: std::collections::HashSet<_> = request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            let result = consume(model.api, response, |delta| {
                events
                    .send(match delta {
                        OutputDelta::Content(text) => ModelDelta::Content(text),
                        OutputDelta::Thinking(text) => ModelDelta::Thinking(text),
                    })
                    .map_err(|_| CANCELLED.to_string())
            })
            .await?;
            if result
                .tool_calls
                .iter()
                .any(|call| !allowed.contains(call.name.as_str()))
            {
                return Err("Grok requested a function that Taceta did not offer. No pending tools were executed.".into());
            }
            Ok(result)
        };
        tokio::select! { _ = events.closed() => Err(CANCELLED.into()), result = work => result }
    }

    async fn chat(
        &self,
        request: ChatRequest,
        events: UnboundedSender<GenerationEvent>,
    ) -> Result<(), String> {
        let work = async {
            if request.web_search_provider.is_some()
                || request.web_authorization.is_some()
                || request.tools.as_ref().is_some_and(|tools| {
                    !tools.is_null() && !tools.as_array().is_some_and(Vec::is_empty)
                })
            {
                return Err("Web search is not available in Grok chat. Use Taceta's agent tools through the explicit agent workflow.".into());
            }
            let model = self.model(&request.model).await?;
            let input = chat_input(&request, model.descriptor.vision, model.api)?;
            let body = payload(
                &request.model,
                &model,
                input,
                &[],
                request.thinking,
                request.context_length,
            )?;
            let started = Instant::now();
            let response = self.response(body, &request.model, model.api).await?;
            let result = consume(model.api, response, |delta| {
                events
                    .send(match delta {
                        OutputDelta::Content(text) => GenerationEvent::ContentDelta(text),
                        OutputDelta::Thinking(text) => GenerationEvent::ThinkingDelta(text),
                    })
                    .map_err(|_| CANCELLED.to_string())
            })
            .await?;
            if !result.tool_calls.is_empty() {
                return Err(
                    "Grok returned function calls in a chat that did not offer tools.".into(),
                );
            }
            events
                .send(GenerationEvent::Completed(GenerationStats {
                    prompt_tokens: result.prompt_tokens,
                    completion_tokens: result.completion_tokens,
                    total_duration_ns: u64::try_from(started.elapsed().as_nanos()).ok(),
                }))
                .map_err(|_| CANCELLED.to_string())
        };
        tokio::select! { _ = events.closed() => Err(CANCELLED.into()), result = work => result }
    }
}

impl InferenceBackend for GrokClient {
    fn list_models(&self) -> BackendFuture<Vec<ModelDescriptor>> {
        let client = self.clone();
        Box::pin(async move { client.fetch_models().await.map_err(BackendError::Grok) })
    }

    fn stream_chat(
        &self,
        request: ChatRequest,
        events: UnboundedSender<GenerationEvent>,
    ) -> BackendFuture<()> {
        let client = self.clone();
        Box::pin(async move {
            client
                .chat(request, events)
                .await
                .map_err(BackendError::Grok)
        })
    }
}

impl AgentModel for GrokClient {
    fn turn(
        &self,
        request: AgentRequest,
        events: UnboundedSender<ModelDelta>,
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurn, String>> + Send>> {
        let client = self.clone();
        Box::pin(async move { client.agent_turn(request, events).await })
    }
}

fn chat_input(request: &ChatRequest, vision: bool, api: ApiKind) -> Result<Vec<Value>, String> {
    let mut input = Vec::new();
    for message in request
        .messages
        .iter()
        .filter(|message| !message.interrupted)
    {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut text = message.content.clone();
        let mut images = Vec::new();
        for attachment in &message.attachments {
            match &attachment.payload {
                AttachmentPayload::Text(body) => {
                    text.push_str(&format!("\n\n[Attachment: {}]\n{body}", attachment.name));
                }
                AttachmentPayload::Image { media_type, base64 } => {
                    if !vision || !matches!(message.role, Role::User) {
                        return Err("Image input is not confirmed for the selected Grok model and message role.".into());
                    }
                    if !matches!(media_type.as_str(), "image/png" | "image/jpeg")
                        || base64.is_empty()
                    {
                        return Err("This image format is not supported by Grok chat.".into());
                    }
                    let url = format!("data:{media_type};base64,{base64}");
                    images.push(match api {
                        ApiKind::Responses => json!({"type": "input_image", "image_url": url}),
                        ApiKind::ChatCompletions => {
                            json!({"type": "image_url", "image_url": {"url": url}})
                        }
                    });
                }
            }
        }
        // message.thinking is intentionally never serialized.
        if images.is_empty() {
            input.push(json!({"role": role, "content": text}));
        } else {
            let kind = match api {
                ApiKind::Responses => "input_text",
                ApiKind::ChatCompletions => "text",
            };
            let mut parts = vec![json!({"type": kind, "text": text})];
            parts.extend(images);
            input.push(json!({"role": role, "content": parts}));
        }
    }
    Ok(input)
}

fn payload(
    name: &str,
    model: &ModelInfo,
    input: Vec<Value>,
    tools: &[ToolDefinition],
    thinking: ThinkingMode,
    context: u32,
) -> Result<Value, String> {
    let mut body = json!({"model": name, "stream": true, "store": false});
    match model.api {
        ApiKind::Responses => {
            body["input"] = Value::Array(input);
            body["max_output_tokens"] = json!(model.max_output(context));
            if !tools.is_empty() {
                body["tools"] = Value::Array(responses::tools(tools)?);
            }
            if let Some(reasoning) = model.reasoning(thinking)? {
                body["reasoning"] = reasoning;
            }
        }
        ApiKind::ChatCompletions => {
            body["messages"] = Value::Array(input);
            body["max_tokens"] = json!(model.max_output(context));
            body["stream_options"] = json!({"include_usage": true});
            if !tools.is_empty() {
                body["tools"] = Value::Array(completions::tools(tools)?);
            }
            if let Some(reasoning) = model.reasoning(thinking)? {
                body["reasoning_effort"] = reasoning["effort"].clone();
            }
        }
    }
    Ok(body)
}

async fn consume(
    api: ApiKind,
    response: reqwest::Response,
    emit: impl FnMut(OutputDelta) -> Result<(), String>,
) -> Result<AgentTurn, String> {
    match api {
        ApiKind::Responses => responses::consume(response.bytes_stream(), emit).await,
        ApiKind::ChatCompletions => completions::consume(response.bytes_stream(), emit).await,
    }
}

fn validate_agent_messages(messages: &[AgentMessage]) -> Result<(), String> {
    let mut pending: HashMap<&str, &str> = HashMap::new();
    let mut ids = HashSet::new();
    for message in messages {
        if matches!(message.role, AgentRole::Tool) {
            let id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or("A Grok tool result has no call ID.")?;
            let name = pending
                .remove(id)
                .ok_or("A Grok tool result has no matching pending call.")?;
            if message
                .tool_name
                .as_deref()
                .is_some_and(|actual| actual != name)
                || !message.tool_calls.is_empty()
            {
                return Err("A Grok tool result does not match its function call.".into());
            }
            continue;
        }
        if !pending.is_empty() {
            return Err(
                "Grok conversation contains a function call without its tool result.".into(),
            );
        }
        if message.tool_call_id.is_some()
            || message.tool_name.is_some()
            || (!matches!(message.role, AgentRole::Assistant) && !message.tool_calls.is_empty())
        {
            return Err("Grok conversation has invalid tool message metadata.".into());
        }
        for call in &message.tool_calls {
            if call.id.is_empty()
                || call.name.is_empty()
                || !call.arguments.is_object()
                || !ids.insert(call.id.as_str())
            {
                return Err(
                    "Grok conversation contains an invalid or repeated function call.".into(),
                );
            }
            pending.insert(&call.id, &call.name);
        }
    }
    if !pending.is_empty() {
        return Err("Grok conversation ends before a pending tool result.".into());
    }
    Ok(())
}

fn validate_tools(tools: &[ToolDefinition]) -> Result<(), String> {
    if tools.len() > responses::MAX_TOOL_CALLS {
        return Err("Grok supports at most 128 tools in one request.".into());
    }
    let mut names = HashSet::new();
    for tool in tools {
        if tool.name.is_empty() || !names.insert(tool.name.as_str()) || !tool.parameters.is_object()
        {
            return Err("The Grok tool definitions are invalid or contain repeated names.".into());
        }
    }
    Ok(())
}

fn reject_status(status: reqwest::StatusCode) -> Result<(), String> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status.as_u16() {
        401 => "Grok rejected Taceta's credential. Connect to Grok again.".into(),
        403 => "Grok did not allow this account or public OAuth client to use the requested service.".into(),
        426 => "Grok rejected this client's compatibility version. Taceta cannot use the service with the current public contract.".into(),
        429 => "Grok's usage limit was reached. Retry after your account's limit resets.".into(),
        code => format!("The Grok service rejected the request (HTTP {code})."),
    })
}

async fn read_bounded(response: reqwest::Response, maximum: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err("Grok returned a response exceeding Taceta's size limit.".into());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "The Grok service response was interrupted.")?;
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err("Grok returned a response exceeding Taceta's size limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(std::mem::take(&mut *bytes))
}

#[cfg(test)]
mod tests;
