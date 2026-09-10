//! Grok transport copied from the MIT grok-codex-bridge src/grok.rs.
//! Taceta preserves catalog display metadata and classifies stream failures
//! without exposing response content or changing retry/completion behavior.
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, ETAG, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use super::auth::SessionCredential;
use super::protocol::{
    NamespaceToolProjection, NormalizedResponsesRequest, ProtocolError, TextStreamState, TextStreamValidator,
    ValidatedTextStreamEvent,
};

const OFFICIAL_INFERENCE_BASE: &str = "https://cli-chat-proxy.grok.com/v1/";
const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_MODE: &str = "headless";
// Match the first-party Grok CLI transport identity.  The bridge remains a
// separate local binary, but cli-chat-proxy gates request compatibility on
// this client family and its lockstepped version.
const CLIENT_IDENTIFIER: &str = "grok-shell";
// `x-grok-client-version` is a cli-chat-proxy compatibility gate, not this
// bridge's package version. Keep the truthful bridge identity in User-Agent
// and `x-grok-client-identifier`; this value tracks xAI's lockstepped
// `xai-grok-version` contract from the admitted Grok Build source snapshot.
const GROK_BUILD_COMPATIBILITY_VERSION: &str = "1.0.5";

#[derive(Clone)]
pub struct GrokClient {
    client: Client,
    base_url: Url,
}

impl GrokClient {
    pub fn production() -> Result<Self, GrokError> {
        let base_url = Url::parse(OFFICIAL_INFERENCE_BASE).expect("official Grok URL is static");
        Self::build(base_url, true)
    }

    fn build(base_url: Url, https_only: bool) -> Result<Self, GrokError> {
        let client = Client::builder()
            .use_rustls_tls()
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(format!("{CLIENT_IDENTIFIER}/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(GrokError::BuildClient)?;
        Ok(Self { client, base_url })
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: Url) -> Result<Self, GrokError> {
        Self::build(base_url, false)
    }

    pub async fn fetch_models(
        &self,
        credential: &SessionCredential,
    ) -> Result<FetchModelsResult, GrokError> {
        let url = self
            .base_url
            .join("models")
            .map_err(GrokError::InvalidEndpoint)?;
        let request = self.authenticated(self.client.get(url), credential);
        let response = tokio::time::timeout(MODELS_TIMEOUT, request.send())
            .await
            .map_err(|_| GrokError::ModelsTimeout)?
            .map_err(GrokError::Transport)?;

        ensure_success(&response)?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let document: ModelsDocument = response.json().await.map_err(GrokError::DecodeModels)?;
        let mut models = Vec::with_capacity(document.data.len());
        for value in &document.data {
            models.push(admit_model(value.clone())?);
        }
        super::catalog::validate_model_ids(models.iter().cloned())?;
        Ok(FetchModelsResult { models, etag, entries: document.data })
    }

    pub async fn post_responses(
        &self,
        credential: Arc<SessionCredential>,
        request: ResponsesTransportRequest<'_>,
    ) -> Result<ResponsesByteStream, GrokError> {
        let conversation_id = request.conversation_id;
        let request_id = request.request_id;
        let agent_id = request.agent_id;
        let turn_index = request.turn_index;
        let prepared = request.prepare()?;
        self.post_prepared_responses(
            credential,
            &prepared,
            conversation_id,
            request_id,
            agent_id,
            turn_index,
        )
        .await
    }

    pub async fn post_prepared_responses(
        &self,
        credential: Arc<SessionCredential>,
        request: &PreparedResponsesRequest,
        conversation_id: Uuid,
        request_id: Uuid,
        agent_id: Uuid,
        turn_index: usize,
    ) -> Result<ResponsesByteStream, GrokError> {
        let url = self
            .base_url
            .join("responses")
            .map_err(GrokError::InvalidEndpoint)?;
        let builder = self
            .authenticated(self.client.post(url), &credential)
            .header(ACCEPT, "text/event-stream")
            .header("x-grok-conv-id", conversation_id.to_string())
            .header("x-grok-req-id", request_id.to_string())
            .header("x-grok-session-id", conversation_id.to_string())
            .header("x-grok-turn-idx", turn_index.to_string())
            .header("x-grok-agent-id", agent_id.to_string())
            .header("x-grok-model-override", &request.model)
            .header(CONTENT_TYPE, "application/json")
            .body(request.body.clone());
        let response = builder.send().await.map_err(GrokError::Transport)?;
        ensure_success(&response)?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(GrokError::UnexpectedResponseContentType);
        }

        let stream = response
            .bytes_stream()
            .map(|item| item.map_err(GrokError::Stream));
        Ok(ResponsesByteStream {
            inner: Box::pin(stream),
            _credential: credential,
            namespace_projection: request.namespace_projection.clone(),
        })
    }

    fn authenticated(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &SessionCredential,
    ) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(credential.token())
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-authenticateresponse", "authenticate-response")
            .header("x-userid", credential.user_id())
            .header("x-grok-user-id", credential.user_id())
            .header("x-grok-client-mode", CLIENT_MODE)
            .header("x-grok-client-identifier", CLIENT_IDENTIFIER)
            .header("x-grok-client-version", GROK_BUILD_COMPATIBILITY_VERSION)
    }
}

pub struct ResponsesTransportRequest<'a> {
    pub body: &'a NormalizedResponsesRequest,
    pub conversation_id: Uuid,
    pub request_id: Uuid,
    pub agent_id: Uuid,
    pub turn_index: usize,
}

pub struct PreparedResponsesRequest {
    body: Bytes,
    model: String,
    namespace_projection: NamespaceToolProjection,
}

impl<'a> ResponsesTransportRequest<'a> {
    pub fn prepare(self) -> Result<PreparedResponsesRequest, GrokError> {
        let model = self.body.model().to_owned();
        let body = serde_json::to_vec(&self.body.to_xai_value())
            .map_err(|_| GrokError::RequestSerialization)?;
        Ok(PreparedResponsesRequest {
            body: Bytes::from(body),
            model,
            namespace_projection: self.body.namespace_projection(),
        })
    }
}

pub struct ResponsesByteStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, GrokError>> + Send>>,
    _credential: Arc<SessionCredential>,
    namespace_projection: NamespaceToolProjection,
}

impl Stream for ResponsesByteStream {
    type Item = Result<Bytes, GrokError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl ResponsesByteStream {
    pub fn validated_text_events(self) -> ValidatedTextEventStream {
        let namespace_projection = self.namespace_projection.clone();
        let data = SseEofBoundaryStream::new(self).eventsource().map(|event| {
            event.map(|event| event.data)
        });
        ValidatedTextEventStream {
            inner: Box::pin(data),
            validator: TextStreamValidator::new(),
            namespace_projection,
            finished: false,
        }
    }
}

/// Terminates a final unterminated SSE block when the upstream closes cleanly.
///
/// Grok can end the response immediately after the final `response.completed`
/// data line. `eventsource-stream` retains that block as incomplete and drops it
/// at EOF, whereas the Grok CLI-compatible parser used by codex-router drains
/// the remaining block. Appending one empty-line boundary preserves the same
/// behavior without treating a genuinely missing terminal event as success.
struct SseEofBoundaryStream<S> {
    inner: S,
    ended: bool,
}

impl<S> SseEofBoundaryStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            ended: false,
        }
    }
}

impl<S> Stream for SseEofBoundaryStream<S>
where
    S: Stream<Item = Result<Bytes, GrokError>> + Unpin,
{
    type Item = Result<Bytes, GrokError>;

    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return std::task::Poll::Ready(None);
        }

        match Pin::new(&mut this.inner).poll_next(context) {
            std::task::Poll::Ready(Some(Err(error))) => {
                this.ended = true;
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                this.ended = true;
                std::task::Poll::Ready(Some(Ok(Bytes::from_static(b"\n\n"))))
            }
            poll => poll,
        }
    }
}

pub struct ValidatedTextEventStream {
    inner: Pin<Box<dyn Stream<Item = Result<String, EventStreamError<GrokError>>> + Send>>,
    validator: TextStreamValidator,
    namespace_projection: NamespaceToolProjection,
    finished: bool,
}

impl Stream for ValidatedTextEventStream {
    type Item = Result<ValidatedTextStreamEvent, GrokError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.finished {
            return std::task::Poll::Ready(None);
        }

        loop {
            match self.inner.as_mut().poll_next(context) {
                std::task::Poll::Ready(Some(Ok(data))) if data.trim().is_empty() => continue,
                std::task::Poll::Ready(Some(Ok(data))) => match self.validator.accept_data(&data) {
                    Ok(mut event) => {
                        event.restore_namespaced_tool_calls(&self.namespace_projection);
                        return std::task::Poll::Ready(Some(Ok(event)));
                    }
                    Err(error) => {
                        self.finished = true;
                        log_rejected_sse_event(&data, &error);
                        return std::task::Poll::Ready(Some(Err(GrokError::Protocol(error))));
                    }
                },
                std::task::Poll::Ready(Some(Err(error))) => {
                    self.finished = true;
                    return std::task::Poll::Ready(Some(Err(GrokError::Sse {
                        failure: SseFailure::from_eventsource(error),
                        state: self.validator.state(),
                    })));
                }
                std::task::Poll::Ready(None) => match self.validator.finish() {
                    Ok(()) => {
                        if let Some(mut event) = self.validator.synthetic_completed_on_eof() {
                            let response_id = event.original()["response"]["id"].as_str();
                            tracing::warn!(
                                route = "responses",
                                response_id,
                                "synthesizing response.completed after upstream EOF"
                            );
                            event.restore_namespaced_tool_calls(&self.namespace_projection);
                            self.finished = true;
                            return std::task::Poll::Ready(Some(Ok(event)));
                        }
                        self.finished = true;
                        return std::task::Poll::Ready(None);
                    }
                    Err(error) => {
                        self.finished = true;
                        return std::task::Poll::Ready(Some(Err(GrokError::Protocol(error))));
                    }
                },
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

fn log_rejected_sse_event(data: &str, error: &ProtocolError) {
    let value = serde_json::from_str::<Value>(data).ok();
    let sse_event_type = value
        .as_ref()
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    let item_type = value
        .as_ref()
        .and_then(|value| value.get("item"))
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str);
    tracing::warn!(
        route = "responses",
        sse_event_type,
        item_type,
        protocol_error = %error,
        "upstream SSE event rejected"
    );
}

pub struct FetchModelsResult {
    pub models: Vec<String>,
    pub etag: Option<String>,
    /// Original admitted catalog entries for Taceta display capabilities only.
    pub entries: Vec<Value>,
}

#[derive(serde::Deserialize)]
struct ModelsDocument {
    data: Vec<Value>,
}

fn admit_model(value: Value) -> Result<String, GrokError> {
    let object = value.as_object().ok_or(GrokError::InvalidModelEntry)?;
    let meta = object.get("_meta").and_then(Value::as_object);
    let model = string_field(object, "model")
        .or_else(|| string_field(object, "modelId"))
        .or_else(|| string_field(object, "id"))
        .or_else(|| meta.and_then(|fields| string_field(fields, "model")))
        .or_else(|| meta.and_then(|fields| string_field(fields, "modelId")))
        .ok_or(GrokError::InvalidModelEntry)?;
    let backend = string_field(object, "apiBackend")
        .or_else(|| string_field(object, "api_backend"))
        .or_else(|| meta.and_then(|fields| string_field(fields, "apiBackend")))
        .or_else(|| meta.and_then(|fields| string_field(fields, "api_backend")))
        .ok_or(GrokError::UnconfirmedResponsesModel)?;
    if backend != "responses" {
        return Err(GrokError::UnconfirmedResponsesModel);
    }
    if bool_field(object, meta, "hidden", "hidden").unwrap_or(false) {
        return Err(GrokError::HiddenModelEntry);
    }
    if !bool_field(object, meta, "supportedInApi", "supported_in_api").unwrap_or(true) {
        return Err(GrokError::UnsupportedApiModelEntry);
    }

    let base_url = string_field(object, "baseUrl")
        .or_else(|| string_field(object, "base_url"))
        .or_else(|| meta.and_then(|fields| string_field(fields, "baseUrl")))
        .or_else(|| meta.and_then(|fields| string_field(fields, "base_url")));
    if base_url
        .is_some_and(|url| normalize_base_url(&url) != normalize_base_url(OFFICIAL_INFERENCE_BASE))
    {
        return Err(GrokError::AlternateModelOrigin);
    }
    Ok(model)
}

fn string_field(fields: &Map<String, Value>, name: &str) -> Option<String> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn bool_field(
    object: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
    camel: &str,
    snake: &str,
) -> Option<bool> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .or_else(|| meta.and_then(|fields| fields.get(camel)))
        .or_else(|| meta.and_then(|fields| fields.get(snake)))
        .and_then(Value::as_bool)
}

fn normalize_base_url(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn ensure_success(response: &reqwest::Response) -> Result<(), GrokError> {
    match response.status() {
        status if status.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Err(GrokError::AuthenticationRejected {
                upstream_status: response.status().as_u16(),
            })
        }
        StatusCode::TOO_MANY_REQUESTS => Err(GrokError::RateLimited {
            retry_after_seconds: response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok()),
        }),
        status => Err(GrokError::UpstreamStatus(status.as_u16())),
    }
}

/// Safe, typed cause of this stream boundary's failure. The parser's input,
/// invalid bytes, underlying error text, and request URL are never retained.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SseFailure {
    #[error("source=utf8; incomplete_codepoint={incomplete_codepoint}")]
    Utf8 { incomplete_codepoint: bool },
    #[error("source=parser; code={code}")]
    Parser { code: String },
    #[error("source=transport; timeout={timeout}; body={body}; decode={decode}")]
    Transport { timeout: bool, body: bool, decode: bool },
    #[error("source=transport; unclassified=true")]
    OtherTransport,
}

impl SseFailure {
    fn from_eventsource(error: EventStreamError<GrokError>) -> Self {
        match error {
            EventStreamError::Utf8(error) => Self::Utf8 {
                incomplete_codepoint: error.utf8_error().error_len().is_none(),
            },
            EventStreamError::Parser(error) => Self::Parser {
                code: error.code.description().to_owned(),
            },
            EventStreamError::Transport(GrokError::Stream(error)) => Self::Transport {
                timeout: error.is_timeout(),
                body: error.is_body(),
                decode: error.is_decode(),
            },
            EventStreamError::Transport(_) => Self::OtherTransport,
        }
    }
}

#[derive(Debug, Error)]
pub enum GrokError {
    #[error("failed to construct the origin-locked xAI client")]
    BuildClient(#[source] reqwest::Error),
    #[error("failed to construct an xAI endpoint URL")]
    InvalidEndpoint(#[source] url::ParseError),
    #[error("xAI transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("xAI model catalog request exceeded its bounded timeout")]
    ModelsTimeout,
    #[error("xAI rejected the session credential; run the official Grok login flow")]
    AuthenticationRejected { upstream_status: u16 },
    #[error("xAI rate limited the request")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("xAI returned unsuccessful status {0}")]
    UpstreamStatus(u16),
    #[error("xAI model catalog response is not valid JSON")]
    DecodeModels(#[source] reqwest::Error),
    #[error("xAI model catalog contains an invalid entry")]
    InvalidModelEntry,
    #[error("xAI model catalog entry is not explicitly backed by Responses")]
    UnconfirmedResponsesModel,
    #[error("xAI model catalog entry is hidden")]
    HiddenModelEntry,
    #[error("xAI model catalog entry is not supported by the API")]
    UnsupportedApiModelEntry,
    #[error("xAI model catalog entry selects an alternate inference origin")]
    AlternateModelOrigin,
    #[error(transparent)]
    Catalog(#[from] super::catalog::CatalogError),
    #[error("xAI Responses transport did not return text/event-stream")]
    UnexpectedResponseContentType,
    // Keep this distinct from Stream: stream classification must not activate
    // retries that the previous SSE error boundary did not perform.
    #[error("xAI Responses stream failed ({failure}; state={state:?})")]
    Sse { failure: SseFailure, state: TextStreamState },
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("xAI Responses stream failed")]
    Stream(#[source] reqwest::Error),
    #[error("failed to serialize the xAI Responses request")]
    RequestSerialization,
}

impl GrokError {
    /// Safe classification for the HTTP response boundary. It deliberately
    /// excludes upstream headers and bodies.
    pub(crate) fn response_boundary_class(&self) -> &'static str {
        match self {
            Self::AuthenticationRejected { .. }
            | Self::RateLimited { .. }
            | Self::UpstreamStatus(_) => "upstream_http_status",
            Self::UnexpectedResponseContentType => "upstream_content_type",
            _ => "upstream_response",
        }
    }

    pub(crate) fn upstream_status(&self) -> Option<u16> {
        match self {
            Self::AuthenticationRejected { upstream_status } => Some(*upstream_status),
            Self::RateLimited { .. } => Some(StatusCode::TOO_MANY_REQUESTS.as_u16()),
            Self::UpstreamStatus(status) => Some(*status),
            _ => None,
        }
    }
}


#[cfg(test)]
pub(super) fn fixture_events(chunks: Vec<Bytes>) -> ValidatedTextEventStream {
    // Same in-memory byte-stream fixture used by the reference grok.rs tests.
    ResponsesByteStream {
        inner: Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok))),
        _credential: Arc::new(SessionCredential::for_test("fixture-token", "fixture-user")),
        namespace_projection: NamespaceToolProjection::default(),
    }.validated_text_events()
}
