//! Taceta's domain adapter around the MIT grok-codex-bridge connection source.
//! OAuth belongs to the official Grok CLI; inference and tool execution stay in Taceta.
mod adapter;
mod auth;
mod catalog;
mod protocol;
mod transport;

use crate::{
    agent::{AgentModel, AgentRequest, AgentTurn, ModelDelta},
    backend::{BackendError, BackendFuture, InferenceBackend},
    domain::{ChatRequest, GenerationEvent, GenerationStats, ModelDescriptor},
};
use adapter::{ModelInfo, OutputDelta};
use auth::AuthManager;
use futures_util::{Stream, StreamExt, stream};
use protocol::{NormalizedResponsesRequest, TextStreamEventKind, ValidatedTextStreamEvent};
use std::{future::Future, pin::Pin, sync::{Arc, RwLock}, time::{Duration, Instant}};
use tokio::sync::mpsc::UnboundedSender;
use transport::{GrokError, ResponsesTransportRequest};

const CANCELLED: &str = "Grok generation was cancelled.";
// Copied from grok-codex-bridge src/server.rs. Retries end before any useful
// downstream output; the prepared body and all routing identities stay fixed.
const EARLY_STREAM_RETRY_LIMIT: usize = 3;
const EARLY_STREAM_RETRY_BACKOFF: [Duration; EARLY_STREAM_RETRY_LIMIT] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];
const EARLY_STREAM_RETRY_WALL_CLOCK: Duration = Duration::from_secs(60);

type TextEvents = Pin<Box<dyn Stream<Item = Result<ValidatedTextStreamEvent, GrokError>> + Send>>;

pub enum GrokLoginEvent {
    OpenBrowser(String),
    Progress(String),
}

#[derive(Clone)]
pub struct GrokClient {
    inner: Arc<Inner>,
}

struct Inner {
    transport: transport::GrokClient,
    auth: AuthManager,
    models: RwLock<Vec<ModelInfo>>,
}

impl Default for GrokClient {
    fn default() -> Self { Self::new() }
}

impl GrokClient {
    /// Construction never reads credentials, starts login, or sends a request.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                transport: transport::GrokClient::production()
                    .expect("the static Grok HTTP client configuration is valid"),
                auth: AuthManager::new(),
                models: RwLock::new(Vec::new()),
            }),
        }
    }

    pub async fn sign_in(&self, events: UnboundedSender<GrokLoginEvent>) -> Result<(), String> {
        self.inner.auth.sign_in(events).await?;
        self.clear_models()
    }

    pub fn sign_out(&self) -> Result<(), String> {
        self.inner.auth.sign_out()?;
        self.clear_models()
    }

    pub fn is_signed_in(&self) -> Result<bool, String> { self.inner.auth.is_signed_in() }

    fn clear_models(&self) -> Result<(), String> {
        self.inner.models.write().map_err(|_| "Grok model access failed.")?.clear();
        Ok(())
    }

    async fn fetch_models(&self) -> Result<Vec<ModelDescriptor>, String> {
        let epoch = self.inner.auth.epoch();
        let credential = self.inner.auth.session_credential().await?;
        let fetched = self.inner.transport.fetch_models(&credential).await.map_err(|e| e.to_string())?;
        let models = fetched.models.into_iter().zip(fetched.entries)
            .map(|(id, entry)| ModelInfo::from_admitted(id, &entry)).collect::<Vec<_>>();
        let descriptors = models.iter().map(|model| model.descriptor.clone()).collect();
        let mut cached = self.inner.models.write().map_err(|_| "Grok model access failed.")?;
        if epoch != self.inner.auth.epoch() {
            return Err("Grok connection changed while models were loading.".into());
        }
        *cached = models;
        Ok(descriptors)
    }

    async fn model(&self, name: &str) -> Result<ModelInfo, String> {
        let cached = self.inner.models.read().map_err(|_| "Grok model access failed.")?
            .iter().find(|model| model.descriptor.name == name).cloned();
        if let Some(model) = cached { return Ok(model); }
        self.fetch_models().await?;
        self.inner.models.read().map_err(|_| "Grok model access failed.")?
            .iter().find(|model| model.descriptor.name == name).cloned()
            .ok_or_else(|| "The selected model is not in this account's Grok inference catalog. Refresh the model list.".into())
    }

    async fn response(&self, request: &NormalizedResponsesRequest) -> Result<TextEvents, String> {
        let credential = self.inner.auth.session_credential().await?;
        start_responses(&self.inner.transport, credential, request).await
    }

    async fn chat(&self, request: ChatRequest, events: UnboundedSender<GenerationEvent>) -> Result<(), String> {
        let work = async {
            if request.web_search_provider.is_some() || request.web_authorization.is_some()
                || request.tools.as_ref().is_some_and(|tools| !tools.is_null() && !tools.as_array().is_some_and(Vec::is_empty)) {
                return Err("Web search is not available in Grok chat. Use Taceta's agent tools through the explicit agent workflow.".into());
            }
            let model = self.model(&request.model).await?;
            let input = adapter::chat_input(&request, model.descriptor.vision)?;
            let normalized = adapter::request(&model, input, &[], request.thinking)?;
            let started = Instant::now();
            let upstream = self.response(&normalized).await?;
            events.send(GenerationEvent::ModelIdentity {
                requested_model: request.model.clone(), reported_model: None,
            }).map_err(|_| CANCELLED.to_string())?;
            let result = adapter::consume(upstream, &[], |delta| {
                let event = match delta {
                    OutputDelta::Content(text) => GenerationEvent::ContentDelta(text),
                    OutputDelta::Thinking(text) => GenerationEvent::ThinkingDelta(text),
                    OutputDelta::ReportedModel(model) => GenerationEvent::ModelIdentity {
                        requested_model: request.model.clone(), reported_model: Some(model),
                    },
                };
                events.send(event).map_err(|_| CANCELLED.to_string())
            }).await?;
            events.send(GenerationEvent::ReplaceContent(result.content))
                .map_err(|_| CANCELLED.to_string())?;
            events.send(GenerationEvent::Completed(GenerationStats {
                prompt_tokens: result.prompt_tokens,
                completion_tokens: result.completion_tokens,
                total_duration_ns: u64::try_from(started.elapsed().as_nanos()).ok(),
            })).map_err(|_| CANCELLED.to_string())
        };
        tokio::select! { _ = events.closed() => Err(CANCELLED.into()), result = work => result }
    }

    async fn agent_turn(&self, request: AgentRequest, events: UnboundedSender<ModelDelta>) -> Result<AgentTurn, String> {
        let work = async {
            let model = self.model(&request.model).await?;
            if !request.tools.is_empty() && !model.descriptor.tools {
                return Err("The selected Grok model does not support function tools.".into());
            }
            let input = adapter::agent_input(&request.messages)?;
            let normalized = adapter::request(&model, input, &request.tools, request.thinking)?;
            let upstream = self.response(&normalized).await?;
            adapter::consume(upstream, &request.tools, |delta| {
                let delta = match delta {
                    OutputDelta::Content(text) => ModelDelta::Content(text),
                    OutputDelta::Thinking(text) => ModelDelta::Thinking(text),
                    OutputDelta::ReportedModel(_) => return Ok(()),
                };
                events.send(delta).map_err(|_| CANCELLED.to_string())
            }).await
        };
        tokio::select! { _ = events.closed() => Err(CANCELLED.into()), result = work => result }
    }
}

impl InferenceBackend for GrokClient {
    fn list_models(&self) -> BackendFuture<Vec<ModelDescriptor>> {
        let client = self.clone();
        Box::pin(async move { client.fetch_models().await.map_err(BackendError::Grok) })
    }
    fn stream_chat(&self, request: ChatRequest, events: UnboundedSender<GenerationEvent>) -> BackendFuture<()> {
        let client = self.clone();
        Box::pin(async move { client.chat(request, events).await.map_err(BackendError::Grok) })
    }
}

impl AgentModel for GrokClient {
    fn turn(&self, request: AgentRequest, events: UnboundedSender<ModelDelta>) -> Pin<Box<dyn Future<Output = Result<AgentTurn, String>> + Send>> {
        let client = self.clone();
        Box::pin(async move { client.agent_turn(request, events).await })
    }
}

/// Direct adaptation of the Grok branch in src/server.rs: prepare once, retry
/// only before a useful event, then hand the same validated stream to Taceta.
async fn start_responses(client: &transport::GrokClient, credential: Arc<auth::SessionCredential>, normalized: &NormalizedResponsesRequest) -> Result<TextEvents, String> {
    let routing = normalized.grok_routing_metadata().map_err(|e| e.to_string())?;
    let prepared = (ResponsesTransportRequest {
        body: normalized, conversation_id: routing.conversation_id(),
        request_id: routing.request_id(), agent_id: routing.agent_id(), turn_index: routing.turn_index(),
    }).prepare().map_err(|e| e.to_string())?;
    let mut early_retries = 0;
    let retry_started = Instant::now();
    let (prelude, upstream) = 'attempt: loop {
        let upstream = match client.post_prepared_responses(
            Arc::clone(&credential), &prepared, routing.conversation_id(),
            routing.request_id(), routing.agent_id(), routing.turn_index(),
        ).await {
            Ok(stream) => stream,
            Err(error) if grok_error_is_transient(&error) && early_retries < EARLY_STREAM_RETRY_LIMIT => {
                let Some(delay) = grok_retry_delay(&error, early_retries, retry_started) else { return Err(error.to_string()); };
                early_retries += 1;
                tokio::time::sleep(delay).await;
                continue 'attempt;
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut upstream = upstream.validated_text_events();
        let mut prelude = Vec::new();
        loop {
            match upstream.next().await {
                Some(Ok(event)) => {
                    let commits_downstream = event_commits_downstream(&event);
                    prelude.push(event);
                    if commits_downstream { break 'attempt (prelude, upstream); }
                }
                Some(Err(error)) if matches!(error, GrokError::Stream(_)) && early_retries < EARLY_STREAM_RETRY_LIMIT => {
                    let Some(delay) = grok_retry_delay(&error, early_retries, retry_started) else { return Err(error.to_string()); };
                    early_retries += 1;
                    tokio::time::sleep(delay).await;
                    continue 'attempt;
                }
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("Grok upstream ended before producing a response.".into()),
            }
        }
    };
    Ok(Box::pin(stream::iter(prelude.into_iter().map(Ok)).chain(upstream)))
}

fn grok_error_is_transient(error: &GrokError) -> bool {
    matches!(error, GrokError::Transport(_) | GrokError::RateLimited { .. } | GrokError::UpstreamStatus(502..=504))
}

fn grok_retry_delay(error: &GrokError, retry_index: usize, started: Instant) -> Option<Duration> {
    let default = EARLY_STREAM_RETRY_BACKOFF[retry_index];
    let requested = match error {
        GrokError::RateLimited { retry_after_seconds: Some(seconds) } => Duration::from_secs(*seconds),
        _ => default,
    };
    (started.elapsed().saturating_add(requested) <= EARLY_STREAM_RETRY_WALL_CLOCK).then_some(requested)
}

fn event_commits_downstream(event: &ValidatedTextStreamEvent) -> bool {
    matches!(event.kind(),
        TextStreamEventKind::OutputTextDelta { .. }
            | TextStreamEventKind::OutputTextDone { .. }
            | TextStreamEventKind::OutputItemDone { .. }
            | TextStreamEventKind::FunctionCallArgumentsDelta { .. }
            | TextStreamEventKind::FunctionCallArgumentsDone { .. }
            | TextStreamEventKind::FunctionCallItemDone { .. }
            | TextStreamEventKind::ReasoningSummaryTextDelta { .. }
            | TextStreamEventKind::ReasoningSummaryTextDone { .. }
            | TextStreamEventKind::ReasoningTextDelta { .. }
            | TextStreamEventKind::ReasoningTextDone { .. }
            | TextStreamEventKind::ReasoningItemDone { .. }
            | TextStreamEventKind::ResponseFailed { .. }
            | TextStreamEventKind::ResponseIncomplete { .. }
            | TextStreamEventKind::ResponseCompleted { .. }
    )
}

#[cfg(test)]
mod tests;
