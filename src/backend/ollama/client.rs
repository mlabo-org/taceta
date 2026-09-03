use super::{
    api::{ChatBody, ChatOptions, PullBody, PullChunk, ShowResponse, TagsResponse, WireMessage},
    capability,
    endpoint::OllamaEndpoint,
    lifecycle, router, stream,
};
use crate::{
    backend::{BackendError, BackendFuture, InferenceBackend, ModelManager},
    domain::{
        AttachmentPayload, ChatMessage, ChatRequest, GenerationEvent, ModelCandidate,
        ModelDescriptor, ModelManagerEvent, ModelPullRequest, Role, ThinkingMode, WebWorkflow,
        normalize_chatgpt_web_request_limit,
    },
    taceta_link_service::{self, ChatGptPromptSource, LinkProgress, LinkResult, TacetaLinkService},
    web_search::{self, ProviderKind, ToolCall, WebSearchProvider},
};
use futures_util::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const MAX_TOOL_CALLS_PER_ROUND: usize = 5;

pub struct OllamaClient {
    http: reqwest::Client,
    endpoint: OllamaEndpoint,
    link_service: Option<Arc<TacetaLinkService>>,
}

impl OllamaClient {
    pub fn new(endpoint: OllamaEndpoint) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint,
            link_service: None,
        }
    }
    pub fn with_link_service(mut self, service: Arc<TacetaLinkService>) -> Self {
        self.link_service = Some(service);
        self
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint.base_url())
    }
    async fn models(&self) -> Result<Vec<ModelDescriptor>, BackendError> {
        lifecycle::ensure_ready(&self.http, &self.endpoint).await?;
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
                context_length: model_context_length(&show.model_info),
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
            lifecycle::ensure_ready(&client.http, &client.endpoint).await?;
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
            let link_workflow = provider_kind.and_then(link_workflow);
            let provider = if link_workflow.is_some() {
                None
            } else {
                tools
                    .as_ref()
                    .map(|_| WebSearchProvider::from_kind(provider_kind.unwrap_or_default()))
                    .transpose()
                    .map_err(|e| BackendError::Protocol(e.to_string()))?
            };
            if link_workflow.is_some() && client.link_service.is_none() {
                return Err(BackendError::Protocol("Taceta Link is unavailable".into()));
            }
            let current_input = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == Role::User)
                .ok_or_else(|| {
                    BackendError::Protocol("Web routing requires a current user input".into())
                })?;
            let route = if tools.is_some() {
                determine_web_route(&client, &request, current_input, &events).await?
            } else {
                router::WebRouteDecision::Local
            };
            let mut messages = wire_conversation_messages(&request.messages);
            if request.fetch_search_pages
                && matches!(
                    &link_workflow,
                    Some(crate::domain::WebWorkflow::DefaultSearch)
                        | Some(crate::domain::WebWorkflow::GoogleSearch)
                )
            {
                messages.push(WireMessage {
                    role: "system".into(),
                    content: "Web検索では、まずweb_searchで候補URLを取得し、その中から信頼できる関連URLを1〜5件選んでweb_fetchで本文を確認してから回答してください。検索結果の見出しやスニペットだけで事実を断定しないでください。".into(),
                    images: Vec::new(),
                    tool_calls: None,
                    tool_name: None,
                });
            }
            let mut seen = HashSet::new();
            let mut fetch_count = 0usize;
            let mut chatgpt_web_budget =
                ChatGptWebRequestBudget::new(request.chatgpt_web_request_limit);
            let initial_search = initial_web_search_call(&request, &route)?;
            let mut forced_search_exhausted_chatgpt_budget = false;
            let mut web_tool_phase_started = false;
            if let Some((call, trigger)) = initial_search {
                forced_search_exhausted_chatgpt_budget = perform_forced_web_search(
                    &client,
                    &request,
                    provider.as_ref(),
                    link_workflow.clone(),
                    Some(current_input),
                    call,
                    &events,
                    &mut chatgpt_web_budget,
                    &mut fetch_count,
                    &mut seen,
                    &mut messages,
                    trigger,
                )
                .await?;
                web_tool_phase_started = true;
            }
            let mut assistant_search_fallback_used = false;
            let local_route = matches!(route, router::WebRouteDecision::Local);
            if tools.is_some() && local_route {
                messages.push(WireMessage {
                    role: "system".into(),
                    content: "このターンは外部検索なしで回答します。検索すると予告せず、利用可能な会話内容から直接回答してください。現在の事実が不確かな場合は推測で補わず、その限界を明示してください。".into(),
                    images: Vec::new(),
                    tool_calls: None,
                    tool_name: None,
                });
            }
            for round in 0..4 {
                if web_tool_phase_started {
                    let _ = events.send(GenerationEvent::SearchProgress(
                        "ローカルモデルで検索結果を確認・要約しています".into(),
                    ));
                }
                let body = ChatBody {
                    model: request.model.clone(),
                    messages: messages.clone(),
                    stream: true,
                    options: ChatOptions::generation(request.context_length),
                    think: think_value(request.thinking),
                    tools: if forced_search_exhausted_chatgpt_budget || local_route {
                        None
                    } else {
                        tools.clone()
                    },
                    format: None,
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
                    if tools.is_some()
                        && !web_tool_phase_started
                        && !assistant_search_fallback_used
                        && web_search::expresses_immediate_search_intent(&streamed.content)
                    {
                        assistant_search_fallback_used = true;
                        let _ = events.send(GenerationEvent::ReplaceContent(String::new()));
                        let call = assistant_intent_web_search_call(&request, &streamed.content)?;
                        forced_search_exhausted_chatgpt_budget = perform_forced_web_search(
                            &client,
                            &request,
                            provider.as_ref(),
                            link_workflow.clone(),
                            Some(current_input),
                            call,
                            &events,
                            &mut chatgpt_web_budget,
                            &mut fetch_count,
                            &mut seen,
                            &mut messages,
                            ForcedSearchTrigger::AssistantIntent,
                        )
                        .await?;
                        web_tool_phase_started = true;
                        continue;
                    }
                    let _ = events.send(GenerationEvent::Completed(streamed.stats));
                    return Ok(());
                }
                if local_route {
                    return Err(BackendError::Protocol(
                        "local web routing produced an undeclared tool call".into(),
                    ));
                }
                if provider.is_none() && link_workflow.is_none() {
                    return Err(BackendError::Protocol(
                        "model returned tool calls while web search is disabled".into(),
                    ));
                }
                let round_budget_exhausted = streamed.tool_calls.len() > MAX_TOOL_CALLS_PER_ROUND;
                messages.push(WireMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    images: Vec::new(),
                    tool_calls: Some(streamed.tool_calls.clone()),
                    tool_name: None,
                });
                let mut chatgpt_web_budget_exhausted = false;
                for (index, raw) in streamed.tool_calls.into_iter().enumerate() {
                    let call = web_search::parse_tool_call(&raw)
                        .map_err(|e| BackendError::Protocol(e.to_string()))?;
                    let (content, urls) = if index >= MAX_TOOL_CALLS_PER_ROUND {
                        let _ = events.send(GenerationEvent::SearchProgress(
                            "このラウンドの検索上限に達しました".into(),
                        ));
                        (
                            budget_payload("このラウンドのtool call上限に達しました"),
                            Vec::new(),
                        )
                    } else {
                        match chatgpt_web_budget.admit(link_workflow.as_ref(), &call.name) {
                            ChatGptWebAdmission::Exhausted => {
                                chatgpt_web_budget_exhausted = true;
                                let _ = events.send(GenerationEvent::SearchProgress(
                                    "Tacetaで設定した質問回数に達したため、収集した回答をまとめています"
                                        .into(),
                                ));
                                (
                                    budget_payload(
                                        "Tacetaで設定したChatGPT Web質問回数に達しました。追加質問は実行せず、取得済みの回答を統合してください",
                                    ),
                                    Vec::new(),
                                )
                            }
                            admission => {
                                let chatgpt_request_ordinal = match admission {
                                    ChatGptWebAdmission::Allowed { ordinal, .. } => Some(ordinal),
                                    ChatGptWebAdmission::Passthrough => None,
                                    ChatGptWebAdmission::Exhausted => unreachable!(),
                                };
                                let result = execute_tool(
                                    provider.as_ref(),
                                    client.link_service.as_ref(),
                                    link_workflow.clone(),
                                    request.web_authorization.clone(),
                                    request.messages.iter().rev().find(|m| m.role == Role::User),
                                    chatgpt_request_ordinal,
                                    ChatGptPromptSource::LocalModelQuery,
                                    &call,
                                    &events,
                                    &mut fetch_count,
                                    request.max_search_results,
                                    request.fetch_search_pages,
                                )
                                .await?;
                                if matches!(
                                    admission,
                                    ChatGptWebAdmission::Allowed {
                                        ordinal: _,
                                        exhausted_after: true
                                    }
                                ) {
                                    chatgpt_web_budget_exhausted = true;
                                }
                                result
                            }
                        }
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
                web_tool_phase_started = true;
                if round == 3 || round_budget_exhausted || chatgpt_web_budget_exhausted {
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
                        options: ChatOptions::generation(request.context_length),
                        think: think_value(request.thinking),
                        tools: None,
                        format: None,
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
            endpoint: self.endpoint.clone(),
            link_service: self.link_service.clone(),
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

fn wire_conversation_messages(messages: &[ChatMessage]) -> Vec<WireMessage> {
    messages
        .iter()
        .filter(|message| message_is_model_context(message))
        .map(wire_message)
        .collect()
}

fn message_is_model_context(message: &ChatMessage) -> bool {
    if message.interrupted {
        return false;
    }
    if message.role != Role::Assistant {
        return true;
    }
    !matches!(
        message.content.trim(),
        "生成を停止しました"
            | "生成を停止しました。"
            | "Generation stopped"
            | "Generation stopped."
    )
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

async fn determine_web_route(
    client: &OllamaClient,
    request: &ChatRequest,
    current_input: &ChatMessage,
    events: &UnboundedSender<GenerationEvent>,
) -> Result<router::WebRouteDecision, BackendError> {
    if web_search::requires_mandatory_search(&current_input.content) {
        return Ok(router::WebRouteDecision::SearchCurrent {
            query: current_input.content.trim().to_owned(),
        });
    }
    let _ = events.send(GenerationEvent::SearchProgress(
        "Web検索の要否を判定中".into(),
    ));
    router::classify(
        &client.http,
        client.url("/api/chat"),
        &request.model,
        &current_input.content,
        request.context_length,
    )
    .await
}

fn initial_web_search_call(
    request: &ChatRequest,
    route: &router::WebRouteDecision,
) -> Result<Option<(ToolCall, ForcedSearchTrigger)>, BackendError> {
    if request.tools.is_none() {
        return Ok(None);
    }
    let (query, trigger) = match route {
        router::WebRouteDecision::Local => return Ok(None),
        router::WebRouteDecision::SearchCurrent { query } => {
            (query.as_str(), ForcedSearchTrigger::CurrentUserInput)
        }
        router::WebRouteDecision::SearchGenerated { query } => {
            (query.as_str(), ForcedSearchTrigger::ModelGeneratedQuery)
        }
    };
    Ok(Some((
        ToolCall {
            name: "web_search".into(),
            arguments: serde_json::json!({
                "query": query,
                "limit": request.max_search_results.clamp(1, 5),
            }),
        },
        trigger,
    )))
}

fn current_input_web_search_call(request: &ChatRequest) -> Result<ToolCall, BackendError> {
    let Some(current_input) = request.messages.iter().rev().find(|m| m.role == Role::User) else {
        return Err(BackendError::Protocol(
            "Web Search requires a current user input".into(),
        ));
    };
    let query = current_input.content.trim();
    if query.is_empty() {
        return Err(BackendError::Protocol(
            "Web Search requires a text question".into(),
        ));
    }
    Ok(ToolCall {
        name: "web_search".into(),
        arguments: serde_json::json!({
            "query": query,
            "limit": request.max_search_results.clamp(1, 5),
        }),
    })
}

fn assistant_intent_web_search_call(
    request: &ChatRequest,
    assistant_output: &str,
) -> Result<ToolCall, BackendError> {
    let mut call = current_input_web_search_call(request)?;
    if let Some(query) = web_search::concrete_search_query_from_assistant(assistant_output) {
        call.arguments["query"] = serde_json::Value::String(query);
    }
    Ok(call)
}

fn wire_tool_call(call: &ToolCall) -> serde_json::Value {
    serde_json::json!({
        "function": {
            "name": call.name.clone(),
            "arguments": call.arguments.clone(),
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForcedSearchTrigger {
    CurrentUserInput,
    ModelGeneratedQuery,
    AssistantIntent,
}

impl ForcedSearchTrigger {
    fn chatgpt_prompt_source(self) -> ChatGptPromptSource {
        match self {
            Self::CurrentUserInput => ChatGptPromptSource::CurrentUserInput,
            Self::ModelGeneratedQuery | Self::AssistantIntent => {
                ChatGptPromptSource::LocalModelQuery
            }
        }
    }
}

async fn perform_forced_web_search(
    client: &OllamaClient,
    request: &ChatRequest,
    provider: Option<&WebSearchProvider>,
    link_workflow: Option<WebWorkflow>,
    current_input: Option<&ChatMessage>,
    call: ToolCall,
    events: &UnboundedSender<GenerationEvent>,
    chatgpt_web_budget: &mut ChatGptWebRequestBudget,
    fetch_count: &mut usize,
    seen: &mut HashSet<String>,
    messages: &mut Vec<WireMessage>,
    trigger: ForcedSearchTrigger,
) -> Result<bool, BackendError> {
    let admission = chatgpt_web_budget.admit(link_workflow.as_ref(), &call.name);
    let (chatgpt_request_ordinal, exhausted_after) = match admission {
        ChatGptWebAdmission::Allowed {
            ordinal,
            exhausted_after,
        } => (Some(ordinal), exhausted_after),
        ChatGptWebAdmission::Passthrough => (None, false),
        ChatGptWebAdmission::Exhausted => {
            return Err(BackendError::Protocol(
                "Taceta-triggered Web Search exhausted its request budget before execution".into(),
            ));
        }
    };
    let (content, urls) = execute_tool(
        provider,
        client.link_service.as_ref(),
        link_workflow,
        request.web_authorization.clone(),
        current_input,
        chatgpt_request_ordinal,
        trigger.chatgpt_prompt_source(),
        &call,
        events,
        fetch_count,
        request.max_search_results,
        request.fetch_search_pages,
    )
    .await?;
    messages.push(WireMessage {
        role: "assistant".into(),
        content: String::new(),
        images: Vec::new(),
        tool_calls: Some(vec![wire_tool_call(&call)]),
        tool_name: None,
    });
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
    let trigger_description = match trigger {
        ForcedSearchTrigger::CurrentUserInput => "現在の入力はWeb検索が必要だと判定されました",
        ForcedSearchTrigger::ModelGeneratedQuery => {
            "ローカルのWeb判定が具体的な検索質問を作成しました"
        }
        ForcedSearchTrigger::AssistantIntent => {
            "ローカルモデルがWeb検索を実行すると発言したため、Tacetaがその発言を表示せず検索として実行しました"
        }
    };
    messages.push(WireMessage {
        role: "system".into(),
        content: if exhausted_after {
            format!(
                "{trigger_description}。直前のtool結果を未検証の外部情報として扱い、結果にない事実を補わず、出典URLを付けて最終回答してください。検索するという予告だけを返してはいけません。追加のtool呼び出しは禁止です。"
            )
        } else {
            format!(
                "{trigger_description}。直前のtool結果を未検証の外部情報として扱い、結果にない事実を補わず、出典URLを付けて回答してください。検索するという予告だけを返してはいけません。追加調査が本当に必要な場合だけ別のtoolを呼び出してください。"
            )
        },
        images: Vec::new(),
        tool_calls: None,
        tool_name: None,
    });
    Ok(exhausted_after)
}

fn budget_payload(message: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "error": message,
        "retryable": false,
    }))
    .unwrap()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChatGptWebAdmission {
    Passthrough,
    Allowed { ordinal: u8, exhausted_after: bool },
    Exhausted,
}

#[derive(Clone, Copy, Debug)]
struct ChatGptWebRequestBudget {
    limit: u8,
    used: u8,
}

impl ChatGptWebRequestBudget {
    fn new(configured_limit: u8) -> Self {
        Self {
            limit: normalize_chatgpt_web_request_limit(configured_limit),
            used: 0,
        }
    }

    fn admit(&mut self, workflow: Option<&WebWorkflow>, tool_name: &str) -> ChatGptWebAdmission {
        if !matches!(workflow, Some(WebWorkflow::ChatGptWeb)) {
            return ChatGptWebAdmission::Passthrough;
        }
        if self.used >= self.limit {
            return ChatGptWebAdmission::Exhausted;
        }
        if tool_name != "web_search" {
            return ChatGptWebAdmission::Passthrough;
        }

        self.used += 1;
        ChatGptWebAdmission::Allowed {
            ordinal: self.used,
            exhausted_after: self.used >= self.limit,
        }
    }
}

fn parse_provider(value: &str) -> Result<ProviderKind, BackendError> {
    match value {
        "Brave" | "brave" => Ok(ProviderKind::Brave),
        "Ollama" | "ollama" => Ok(ProviderKind::Ollama),
        "Default Browser Search" | "default_search" => Ok(ProviderKind::DefaultSearch),
        "Google Search" | "google_search" => Ok(ProviderKind::GoogleSearch),
        "ChatGPT Web" | "chatgpt_web" => Ok(ProviderKind::ChatGptWeb),
        _ => Err(BackendError::Protocol(
            "unsupported web search provider".into(),
        )),
    }
}

fn link_workflow(provider: ProviderKind) -> Option<crate::domain::WebWorkflow> {
    match provider {
        ProviderKind::DefaultSearch => Some(crate::domain::WebWorkflow::DefaultSearch),
        ProviderKind::GoogleSearch => Some(crate::domain::WebWorkflow::GoogleSearch),
        ProviderKind::ChatGptWeb => Some(crate::domain::WebWorkflow::ChatGptWeb),
        _ => None,
    }
}

async fn execute_tool(
    provider: Option<&WebSearchProvider>,
    link_service: Option<&Arc<TacetaLinkService>>,
    link_workflow: Option<crate::domain::WebWorkflow>,
    authorization: Option<crate::domain::WebAuthorization>,
    current_input: Option<&ChatMessage>,
    chatgpt_request_ordinal: Option<u8>,
    chatgpt_prompt_source: ChatGptPromptSource,
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
            if let Some(workflow) = link_workflow {
                let service = link_service
                    .ok_or_else(|| BackendError::Protocol("Taceta Link is unavailable".into()))?;
                let auth = authorization.ok_or_else(|| {
                    BackendError::Protocol("Taceta Link authorization is missing".into())
                })?;
                let ordinal = chatgpt_request_ordinal.unwrap_or(1);
                let progress = if matches!(workflow, WebWorkflow::ChatGptWeb) {
                    if ordinal == 1 {
                        "ChatGPT Webで調査中 (1回目)".to_owned()
                    } else {
                        format!(
                            "ChatGPT Webで追加調査中 ({ordinal}回目) — ローカルモデル案: {query}"
                        )
                    }
                } else {
                    format!("検索中: {query}")
                };
                let _ = events.send(GenerationEvent::SearchProgress(progress));
                let job = taceta_link_service::job_for_workflow(
                    workflow.clone(),
                    Some(query),
                    current_input,
                    ordinal,
                    chatgpt_prompt_source,
                    auth,
                )
                .map_err(|e| BackendError::Protocol(e.to_string()))?;
                let wait_ms = link_wait_duration(job.timeout_ms);
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
                let wait = service.enqueue_and_wait_with_progress(job, progress_tx);
                let deadline = tokio::time::sleep(wait_ms);
                tokio::pin!(wait);
                tokio::pin!(deadline);
                let mut streamed_answer = String::new();
                let mut last_sequence = 0;
                let outcome = loop {
                    tokio::select! {
                        result = &mut wait => break result,
                        _ = &mut deadline => break Err(taceta_link_service::LinkError::Timeout),
                        progress = progress_rx.recv() => {
                            if let Some(progress) = progress {
                                apply_link_progress(progress, &mut last_sequence, &mut streamed_answer, events);
                            }
                        }
                    }
                };
                while let Ok(progress) = progress_rx.try_recv() {
                    apply_link_progress(progress, &mut last_sequence, &mut streamed_answer, events);
                }
                let result = match outcome {
                    Ok(result) => result,
                    Err(error) if !streamed_answer.is_empty() => {
                        let _ = events.send(GenerationEvent::SearchProgress(
                            "ChatGPT Webの受信済み回答を使用します".into(),
                        ));
                        LinkResult {
                            workflow: workflow.clone(),
                            data: serde_json::json!({
                                "answer": streamed_answer,
                                "citations": [],
                                "partial": true,
                                "completion_error": error.to_string(),
                            }),
                            mutation_state: crate::browser_harness::MutationState::Performed,
                            lifecycle: Some(crate::browser_harness::LifecycleState::Failed),
                        }
                    }
                    Err(taceta_link_service::LinkError::Timeout) => {
                        return Err(BackendError::Protocol(format!(
                            "Taceta Link {} request timed out",
                            workflow_wire_name(workflow),
                        )));
                    }
                    Err(error) => return Err(BackendError::Protocol(error.to_string())),
                };
                let urls = result.citation_urls();
                return Ok((result.untrusted_context(), urls));
            }
            let _ = events.send(GenerationEvent::SearchProgress(format!("検索中: {query}")));
            let provider = provider
                .ok_or_else(|| BackendError::Protocol("web provider is unavailable".into()))?;
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
            let requested_url = call.arguments["url"]
                .as_str()
                .ok_or_else(|| BackendError::Protocol("web_fetch requires a URL".into()))?;
            let url = web_search::validate_public_url(requested_url)
                .map_err(|error| BackendError::Protocol(error.to_string()))?;
            if url.scheme() != "https" {
                return Err(BackendError::Protocol(
                    "web_fetch requires a public HTTPS URL".into(),
                ));
            }
            *fetch_count += 1;
            let url = url.to_string();
            let _ = events.send(GenerationEvent::SearchProgress(format!("取得中: {url}")));
            if let Some(workflow) = link_workflow {
                if !matches!(
                    workflow,
                    crate::domain::WebWorkflow::DefaultSearch
                        | crate::domain::WebWorkflow::GoogleSearch
                ) {
                    return Err(BackendError::Protocol(
                        "the selected browser workflow cannot read external pages".into(),
                    ));
                }
                let service = link_service
                    .ok_or_else(|| BackendError::Protocol("Taceta Link is unavailable".into()))?;
                let auth = authorization.ok_or_else(|| {
                    BackendError::Protocol("Taceta Link authorization is missing".into())
                })?;
                let job = taceta_link_service::page_fetch_job(url.clone(), auth)
                    .map_err(|error| BackendError::Protocol(error.to_string()))?;
                let wait_ms = link_wait_duration(job.timeout_ms);
                return match tokio::time::timeout(wait_ms, service.enqueue_and_wait(job)).await {
                    Ok(Ok(result)) => {
                        let urls = result.citation_urls();
                        Ok((result.untrusted_context(), urls))
                    }
                    Ok(Err(error)) => {
                        let _ = events
                            .send(GenerationEvent::SearchProgress(format!("取得失敗: {url}")));
                        let payload = serde_json::json!({
                            "error": "ページを取得できませんでした",
                            "url": url,
                            "retryable": false,
                            "detail": error.to_string(),
                        });
                        Ok((serde_json::to_string(&payload).unwrap(), Vec::new()))
                    }
                    Err(_) => {
                        let _ = events
                            .send(GenerationEvent::SearchProgress(format!("取得失敗: {url}")));
                        let payload = serde_json::json!({
                            "error": "ページの取得が時間切れになりました",
                            "url": url,
                            "retryable": true,
                        });
                        Ok((serde_json::to_string(&payload).unwrap(), Vec::new()))
                    }
                };
            }
            let provider = provider
                .ok_or_else(|| BackendError::Protocol("web provider is unavailable".into()))?;
            match provider.fetch(&url).await {
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

fn apply_link_progress(
    progress: LinkProgress,
    last_sequence: &mut u64,
    answer: &mut String,
    events: &UnboundedSender<GenerationEvent>,
) {
    if progress.sequence <= *last_sequence {
        return;
    }
    let replace = progress.replace || progress.sequence != last_sequence.saturating_add(1);
    if replace {
        answer.clear();
    }
    answer.push_str(&progress.delta);
    *last_sequence = progress.sequence;
    let _ = events.send(GenerationEvent::ExternalContentDelta {
        delta: progress.delta,
        replace,
    });
}

fn link_wait_duration(timeout_ms: u64) -> Duration {
    // The extension owns workflow timing. Keep a separate bounded allowance
    // for Native Messaging result delivery after the browser finishes.
    Duration::from_millis(timeout_ms.saturating_add(5_000))
}

fn workflow_wire_name(workflow: crate::domain::WebWorkflow) -> &'static str {
    match workflow {
        crate::domain::WebWorkflow::DefaultSearch => "default_search",
        crate::domain::WebWorkflow::GoogleSearch => "google_search",
        crate::domain::WebWorkflow::PageFetch => "page_fetch",
        crate::domain::WebWorkflow::ChatGptWeb => "chatgpt_web",
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

fn model_context_length(
    model_info: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<u32> {
    model_info
        .iter()
        .filter(|(key, _)| key.ends_with(".context_length"))
        .filter_map(|(_, value)| value.as_u64())
        .filter_map(|value| u32::try_from(value).ok())
        .max()
}

pub struct OllamaModelManager {
    http: reqwest::Client,
    endpoint: OllamaEndpoint,
}

impl OllamaModelManager {
    pub fn new(endpoint: OllamaEndpoint) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint.base_url())
    }
    fn clone_for_task(&self) -> Self {
        Self {
            http: self.http.clone(),
            endpoint: self.endpoint.clone(),
        }
    }
    async fn installed(&self) -> Result<Vec<ModelDescriptor>, BackendError> {
        lifecycle::ensure_ready(&self.http, &self.endpoint).await?;
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
            result.push(ModelDescriptor {
                name: name.clone(),
                size: model.size,
                thinking: capability::classify(&name, &show.details.family),
                vision: capability::has_vision(&show.capabilities),
                tools: capability::has_tools(&show.capabilities),
                context_length: model_context_length(&show.model_info),
            });
        }
        Ok(result)
    }

    async fn available(&self, model: &str) -> Result<Vec<ModelCandidate>, BackendError> {
        let base = catalog_model_base(model)?;
        let model_url = format!("https://ollama.com/library/{base}");
        let tags_url = format!("{model_url}/tags");
        let (model_response, tags_response) = tokio::join!(
            self.http
                .get(model_url)
                .header(reqwest::header::CACHE_CONTROL, "no-cache")
                .send(),
            self.http
                .get(tags_url)
                .header(reqwest::header::CACHE_CONTROL, "no-cache")
                .send()
        );
        let model_response = model_response?;
        let tags_response = tags_response?;
        if model_response.status() == reqwest::StatusCode::NOT_FOUND
            || tags_response.status() == reqwest::StatusCode::NOT_FOUND
        {
            return Err(BackendError::Protocol(format!(
                "model not found in Ollama Library: {base}"
            )));
        }
        let model_html = model_response.error_for_status()?.text().await?;
        let tags_html = tags_response.error_for_status()?.text().await?;
        let preferred = parse_library_recommendation(&model_html, &base);
        let candidates = parse_library_candidates(&tags_html, &base, preferred.as_deref());
        validate_library_candidate_count(&tags_html, candidates.len(), &base)?;
        if candidates.is_empty() {
            return Err(BackendError::Protocol(format!(
                "no downloadable tags found in Ollama Library for {base}"
            )));
        }
        Ok(candidates)
    }
}

impl ModelManager for OllamaModelManager {
    fn list_installed(&self) -> BackendFuture<Vec<ModelDescriptor>> {
        let manager = self.clone_for_task();
        Box::pin(async move { manager.installed().await })
    }
    fn list_available(&self, model: String) -> BackendFuture<Vec<ModelCandidate>> {
        let manager = self.clone_for_task();
        Box::pin(async move { manager.available(&model).await })
    }
    fn pull(
        &self,
        request: ModelPullRequest,
        events: UnboundedSender<ModelManagerEvent>,
    ) -> BackendFuture<()> {
        let manager = self.clone_for_task();
        Box::pin(async move {
            let model = request.model.trim().to_owned();
            if model.is_empty() {
                return Err(BackendError::Protocol("model name is empty".into()));
            }
            lifecycle::ensure_ready(&manager.http, &manager.endpoint).await?;
            let _ = events.send(ModelManagerEvent::Started {
                model: model.clone(),
            });
            let response = manager
                .http
                .post(manager.url("/api/pull"))
                .json(&PullBody {
                    model: model.clone(),
                    stream: true,
                })
                .send()
                .await?
                .error_for_status()?;
            let mut pending = Vec::new();
            let mut body = response.bytes_stream();
            while let Some(chunk) = body.next().await {
                pending.extend_from_slice(&chunk?);
                while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    if let Some(event) = parse_pull_line(&line, &model)? {
                        let done = matches!(event, ModelManagerEvent::Completed { .. });
                        let _ = events.send(event);
                        if done {
                            return Ok(());
                        }
                    }
                }
            }
            if !pending.is_empty() {
                if let Some(event) = parse_pull_line(&pending, &model)? {
                    let _ = events.send(event);
                }
            }
            Ok(())
        })
    }
    fn delete(&self, model: String) -> BackendFuture<()> {
        let manager = self.clone_for_task();
        Box::pin(async move {
            let model = model.trim().to_owned();
            if model.is_empty() {
                return Err(BackendError::Protocol("model name is empty".into()));
            }
            lifecycle::ensure_ready(&manager.http, &manager.endpoint).await?;
            manager
                .http
                .delete(manager.url("/api/delete"))
                .json(&serde_json::json!({"model": model}))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        })
    }
}

fn catalog_model_base(model: &str) -> Result<String, BackendError> {
    let base = model
        .trim()
        .split_once(':')
        .map_or(model.trim(), |(base, _)| base);
    if base.is_empty()
        || !base.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
    {
        return Err(BackendError::Protocol(
            "model name contains unsupported catalog characters".into(),
        ));
    }
    Ok(base.to_owned())
}

fn parse_library_recommendation(html: &str, base: &str) -> Option<String> {
    let marker = "ollama run ";
    let start = html.find(marker)? + marker.len();
    let model = html[start..]
        .split(|character: char| character == '<' || character.is_whitespace())
        .next()?
        .trim();
    if model == base {
        Some(format!("{base}:latest"))
    } else if model.starts_with(&format!("{base}:")) {
        Some(model.to_owned())
    } else {
        None
    }
}

fn parse_library_candidates(
    html: &str,
    base: &str,
    preferred: Option<&str>,
) -> Vec<ModelCandidate> {
    const INPUT_MARKER: &str = "<input class=\"command hidden\" value=\"";
    let prefix = format!("{base}:");
    let mut candidates = Vec::new();
    for section in html.split(INPUT_MARKER).skip(1) {
        let Some(end) = section.find('"') else {
            continue;
        };
        let model = &section[..end];
        if !model.starts_with(&prefix)
            || candidates
                .iter()
                .any(|entry: &ModelCandidate| entry.model == model)
        {
            continue;
        }
        add_library_candidate(
            &mut candidates,
            model,
            parse_library_size(&section[end..]),
            base,
            preferred,
        );
    }

    // The command input is a desktop-only copy affordance and is not the
    // authoritative tag inventory. Collect the actual tag links as well so a
    // partial/responsive rendering cannot silently reduce the available list.
    let link_marker = format!("href=\"/library/{base}:");
    for section in html.split(&link_marker).skip(1) {
        let Some(end) = section.find('"') else {
            continue;
        };
        let model = format!("{base}:{}", &section[..end]);
        add_library_candidate(&mut candidates, &model, None, base, preferred);
    }
    candidates.sort_by_key(|candidate| !candidate.recommended);
    candidates
}

fn add_library_candidate(
    candidates: &mut Vec<ModelCandidate>,
    model: &str,
    estimated_size: Option<String>,
    base: &str,
    preferred: Option<&str>,
) {
    let prefix = format!("{base}:");
    if !model.starts_with(&prefix) {
        return;
    }
    if let Some(candidate) = candidates.iter_mut().find(|entry| entry.model == model) {
        if candidate.estimated_size.is_none() {
            candidate.estimated_size = estimated_size;
        }
        return;
    }
    candidates.push(ModelCandidate {
        model: model.to_owned(),
        estimated_size,
        recommended: preferred.is_some_and(|preferred| preferred == model)
            || (preferred.is_none() && model == format!("{base}:latest")),
    });
}

fn parse_library_size(section: &str) -> Option<String> {
    const SIZE_MARKER: &str = "<p class=\"col-span-2 text-neutral-500 text-[13px]\"";
    let marker = section.find(SIZE_MARKER)?;
    let start = section[marker..].find('>')? + marker + 1;
    let end = section[start..].find("</p>")? + start;
    let size = section[start..end].trim();
    (!size.is_empty()).then(|| size.to_owned())
}

fn parse_library_declared_count(html: &str) -> Option<usize> {
    html.match_indices(" models</p>").find_map(|(end, _)| {
        let before = &html[..end];
        let start = before.rfind('>')? + 1;
        before[start..].trim().parse().ok()
    })
}

fn validate_library_candidate_count(
    html: &str,
    parsed_count: usize,
    base: &str,
) -> Result<(), BackendError> {
    if let Some(declared_count) = parse_library_declared_count(html) {
        if parsed_count < declared_count {
            return Err(BackendError::Protocol(format!(
                "incomplete Ollama Library tag list for {base}: expected {declared_count}, parsed {parsed_count}"
            )));
        }
    }
    Ok(())
}

fn parse_pull_line(line: &[u8], model: &str) -> Result<Option<ModelManagerEvent>, BackendError> {
    let line = std::str::from_utf8(line)
        .map_err(|e| BackendError::Protocol(e.to_string()))?
        .trim();
    if line.is_empty() {
        return Ok(None);
    }
    let chunk: PullChunk = serde_json::from_str(line)
        .map_err(|e| BackendError::Protocol(format!("invalid pull response: {e}")))?;
    if let Some(error) = chunk.error {
        return Err(BackendError::Protocol(error));
    }
    if chunk.status == "success" {
        return Ok(Some(ModelManagerEvent::Completed {
            model: model.to_owned(),
        }));
    }
    Ok(Some(ModelManagerEvent::Progress {
        status: chunk.status,
        completed: chunk.completed,
        total: chunk.total,
    }))
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
            options: ChatOptions::generation(4096),
            think: think_value(ThinkingMode::Default),
            tools: None,
            format: None,
        };
        let json = serde_json::to_value(body).unwrap();
        assert!(!json.as_object().unwrap().contains_key("think"));
        assert_eq!(json["options"]["num_ctx"], serde_json::json!(4096));
    }

    #[test]
    fn model_context_length_uses_model_capacity_not_rope_original_length() {
        let model_info = std::collections::HashMap::from([
            (
                "gptoss.context_length".to_owned(),
                serde_json::json!(131_072),
            ),
            (
                "gptoss.rope.scaling.original_context_length".to_owned(),
                serde_json::json!(4_096),
            ),
        ]);
        assert_eq!(model_context_length(&model_info), Some(131_072));
    }

    #[test]
    fn browser_wait_uses_job_deadline_plus_bounded_transport_grace() {
        assert_eq!(link_wait_duration(30_000), Duration::from_millis(35_000));
        assert_eq!(link_wait_duration(120_000), Duration::from_millis(125_000));
        assert_eq!(
            link_wait_duration(u64::MAX),
            Duration::from_millis(u64::MAX)
        );
    }

    #[test]
    fn chatgpt_progress_is_assembled_in_taceta_and_rewrites_when_required() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let job_id = uuid::Uuid::new_v4();
        let mut sequence = 0;
        let mut answer = String::new();
        apply_link_progress(
            LinkProgress {
                job_id,
                workflow: crate::domain::WebWorkflow::ChatGptWeb,
                sequence: 1,
                delta: "回".into(),
                replace: false,
            },
            &mut sequence,
            &mut answer,
            &events_tx,
        );
        apply_link_progress(
            LinkProgress {
                job_id,
                workflow: crate::domain::WebWorkflow::ChatGptWeb,
                sequence: 2,
                delta: "答".into(),
                replace: false,
            },
            &mut sequence,
            &mut answer,
            &events_tx,
        );
        assert_eq!(answer, "回答");
        assert!(matches!(
            events_rx.try_recv().unwrap(),
            GenerationEvent::ExternalContentDelta { delta, replace: false } if delta == "回"
        ));
        assert!(matches!(
            events_rx.try_recv().unwrap(),
            GenerationEvent::ExternalContentDelta { delta, replace: false } if delta == "答"
        ));
        apply_link_progress(
            LinkProgress {
                job_id,
                workflow: crate::domain::WebWorkflow::ChatGptWeb,
                sequence: 3,
                delta: "修正版".into(),
                replace: true,
            },
            &mut sequence,
            &mut answer,
            &events_tx,
        );
        assert_eq!(answer, "修正版");
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
    fn structured_route_selects_the_initial_search_without_reading_history() {
        let tools = Some(web_search::tool_definitions());
        let mut request = ChatRequest {
            model: "qwen3.8".into(),
            messages: vec![
                ChatMessage::new_user("最新情報をWeb検索して"),
                ChatMessage::new_assistant("承知しました"),
                ChatMessage::new_user("今日は雑談しよう"),
            ],
            thinking: ThinkingMode::Default,
            context_length: 4096,
            tools: tools.clone(),
            web_search_provider: Some("chatgpt_web".into()),
            max_search_results: 5,
            chatgpt_web_request_limit: 1,
            fetch_search_pages: false,
            web_authorization: None,
        };
        assert!(
            initial_web_search_call(&request, &router::WebRouteDecision::Local)
                .unwrap()
                .is_none()
        );

        let (call, trigger) = initial_web_search_call(
            &request,
            &router::WebRouteDecision::SearchCurrent {
                query: "2026年9月3日時点の情報を、出典元URL付きで教えて".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(trigger, ForcedSearchTrigger::CurrentUserInput);
        assert_eq!(call.name, "web_search");
        assert_eq!(
            call.arguments["query"],
            "2026年9月3日時点の情報を、出典元URL付きで教えて"
        );

        request.tools = None;
        assert!(
            initial_web_search_call(
                &request,
                &router::WebRouteDecision::SearchCurrent {
                    query: "ignored because Web is OFF".into(),
                },
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn assistant_intent_search_uses_the_generated_question() {
        let request = ChatRequest {
            model: "qwen3.8".into(),
            messages: vec![ChatMessage::new_user("別の質問を投げてみて")],
            thinking: ThinkingMode::Default,
            context_length: 4096,
            tools: Some(web_search::tool_definitions()),
            web_search_provider: Some("chatgpt_web".into()),
            max_search_results: 5,
            chatgpt_web_request_limit: 1,
            fetch_search_pages: false,
            web_authorization: None,
        };
        let call = assistant_intent_web_search_call(
            &request,
            "では別の質問です。『2026年に注目されるオープンソースAIモデルは何ですか？』をWeb検索します。",
        )
        .unwrap();
        assert_eq!(
            call.arguments["query"],
            "2026年に注目されるオープンソースAIモデルは何ですか？"
        );

        let fallback =
            assistant_intent_web_search_call(&request, "承知しました。WEBサーチを実行します。")
                .unwrap();
        assert_eq!(fallback.arguments["query"], "別の質問を投げてみて");
    }

    #[test]
    fn interrupted_assistant_output_never_reenters_model_context() {
        let mut interrupted = ChatMessage::new_assistant("partial answer");
        interrupted.interrupted = true;
        let messages = vec![
            ChatMessage::new_user("first question"),
            interrupted,
            ChatMessage::new_assistant("生成を停止しました"),
            ChatMessage::new_user("current question"),
        ];
        let wired = wire_conversation_messages(&messages);
        assert_eq!(wired.len(), 2);
        assert_eq!(wired[0].content, "first question");
        assert_eq!(wired[1].content, "current question");
    }

    #[test]
    fn chatgpt_web_budget_defaults_to_one_and_caps_requests_at_three() {
        let mut default_budget = ChatGptWebRequestBudget::new(0);
        assert_eq!(
            default_budget.admit(Some(&WebWorkflow::ChatGptWeb), "web_search"),
            ChatGptWebAdmission::Allowed {
                ordinal: 1,
                exhausted_after: true
            }
        );
        assert_eq!(
            default_budget.admit(Some(&WebWorkflow::ChatGptWeb), "web_search"),
            ChatGptWebAdmission::Exhausted
        );

        let mut maximum_budget = ChatGptWebRequestBudget::new(9);
        for (ordinal, remaining) in [(1, false), (2, false), (3, true)] {
            assert_eq!(
                maximum_budget.admit(Some(&WebWorkflow::ChatGptWeb), "web_search"),
                ChatGptWebAdmission::Allowed {
                    ordinal,
                    exhausted_after: remaining
                }
            );
        }
        assert_eq!(
            maximum_budget.admit(Some(&WebWorkflow::ChatGptWeb), "web_search"),
            ChatGptWebAdmission::Exhausted
        );
    }

    #[test]
    fn chatgpt_web_budget_does_not_limit_search_engine_workflows() {
        let mut budget = ChatGptWebRequestBudget::new(1);
        for _ in 0..4 {
            assert_eq!(
                budget.admit(Some(&WebWorkflow::GoogleSearch), "web_search"),
                ChatGptWebAdmission::Passthrough
            );
        }
        assert_eq!(budget.used, 0);
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

    #[test]
    fn pull_wire_body_requests_a_stream_for_the_selected_model() {
        let value = serde_json::to_value(PullBody {
            model: "qwen3:8b".into(),
            stream: true,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"model":"qwen3:8b", "stream":true})
        );
    }

    #[test]
    fn pull_ndjson_parser_handles_progress_completion_and_errors() {
        let progress = parse_pull_line(
            br#"{"status":"downloading","completed":5,"total":10}"#,
            "qwen3:8b",
        )
        .unwrap();
        assert_eq!(
            progress,
            Some(ModelManagerEvent::Progress {
                status: "downloading".into(),
                completed: Some(5),
                total: Some(10)
            })
        );
        let done = parse_pull_line(br#"{"status":"success"}"#, "qwen3:8b").unwrap();
        assert_eq!(
            done,
            Some(ModelManagerEvent::Completed {
                model: "qwen3:8b".into()
            })
        );
        assert!(parse_pull_line(br#"{"error":"not found"}"#, "qwen3:8b").is_err());
        assert!(parse_pull_line(br#"{"status":"downloading"}"#, "qwen3:8b").is_ok());
    }

    #[test]
    fn library_catalog_parses_sizes_and_prioritizes_the_recommended_tag() {
        let model_html = "<pre>ollama run qwen3.8:27b-mlx</pre>";
        let tags_html = r#"
            <input class="command hidden" value="qwen3.8:latest" />
            <p class="col-span-2 text-neutral-500 text-[13px]">18GB</p>
            <input class="command hidden" value="qwen3.8:27b-mlx" />
            <p class="col-span-2 text-neutral-500 text-[13px]">18GB</p>
        "#;
        let preferred = parse_library_recommendation(model_html, "qwen3.8");
        let candidates = parse_library_candidates(tags_html, "qwen3.8", preferred.as_deref());
        assert_eq!(preferred.as_deref(), Some("qwen3.8:27b-mlx"));
        assert_eq!(candidates[0].model, "qwen3.8:27b-mlx");
        assert_eq!(candidates[0].estimated_size.as_deref(), Some("18GB"));
        assert!(candidates[0].recommended);
        assert!(!candidates[1].recommended);
    }

    #[test]
    fn library_base_command_recommends_latest_when_that_tag_exists() {
        let model_html = "<pre>ollama run qwen3.8</pre>";
        let tags_html = r#"
            <input class="command hidden" value="qwen3.8:latest" />
            <p class="col-span-2 text-neutral-500 text-[13px]">18GB</p>
        "#;
        let preferred = parse_library_recommendation(model_html, "qwen3.8");
        let candidates = parse_library_candidates(tags_html, "qwen3.8", preferred.as_deref());
        assert_eq!(preferred.as_deref(), Some("qwen3.8:latest"));
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].recommended);
    }

    #[test]
    fn library_catalog_recovers_tags_from_links_when_command_inputs_are_partial() {
        let tags_html = r#"
            <p class="block col-span-6 md:hidden">3 models</p>
            <a href="/library/gemma4:latest">gemma4:latest</a>
            <input class="command hidden" value="gemma4:latest" />
            <p class="col-span-2 text-neutral-500 text-[13px]">9.6GB</p>
            <a href="/library/gemma4:cloud">gemma4:cloud</a>
            <input class="command hidden" value="gemma4:cloud" />
            <p class="col-span-2 text-neutral-500 text-[13px]">Low Usage</p>
            <a href="/library/gemma4:26b-mlx">gemma4:26b-mlx</a>
        "#;
        let candidates = parse_library_candidates(tags_html, "gemma4", Some("gemma4:latest"));
        assert_eq!(candidates.len(), 3);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.model == "gemma4:26b-mlx")
        );
        assert!(validate_library_candidate_count(tags_html, candidates.len(), "gemma4").is_ok());
    }

    #[test]
    fn library_catalog_rejects_a_silently_truncated_candidate_list() {
        let tags_html = r#"<p class="block col-span-6 md:hidden">50 models</p>"#;
        assert!(validate_library_candidate_count(tags_html, 2, "gemma4").is_err());
    }
}
