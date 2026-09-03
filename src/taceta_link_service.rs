//! App-owned client for the browser extension's Native Messaging bridge.
//! The browser is an executor only: returned page text is untrusted context.

#[cfg(test)]
use crate::browser_harness::default_socket_path;
use crate::browser_harness::{Envelope, LifecycleState, MutationState, Operation, ProtocolError};
use crate::domain::{ChatMessage, Role, WebAuthorization, WebWorkflow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{BufReader, BufWriter},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkJob {
    pub job_id: Uuid,
    pub workflow: WebWorkflow,
    /// Search workflows use query, page fetch uses url, and ChatGPT Web uses
    /// prompt. Exactly one input is present for every executable job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub authorization: WebAuthorization,
    pub limit: u8,
    /// Absolute safety ceiling owned by the whole browser job.
    pub timeout_ms: u64,
    /// Sliding no-progress limit. ChatGPT Web refreshes this while generation
    /// or visible response activity continues.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
}

const CHATGPT_WEB_HARD_TIMEOUT_MS: u64 = 20 * 60 * 1_000;
const CHATGPT_WEB_IDLE_TIMEOUT_MS: u64 = 3 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatGptPromptSource {
    CurrentUserInput,
    LocalModelQuery,
}

impl LinkJob {
    pub fn search(
        workflow: WebWorkflow,
        query: impl Into<String>,
        authorization: WebAuthorization,
    ) -> Result<Self, LinkError> {
        if !matches!(
            workflow,
            WebWorkflow::DefaultSearch | WebWorkflow::GoogleSearch
        ) {
            return Err(LinkError::InvalidJob("search workflow required"));
        }
        let query = query.into();
        if query.trim().is_empty() {
            return Err(LinkError::InvalidJob("query is empty"));
        }
        Ok(Self {
            job_id: Uuid::new_v4(),
            workflow,
            query: Some(query),
            url: None,
            prompt: None,
            authorization,
            limit: 5,
            timeout_ms: 30_000,
            idle_timeout_ms: None,
        })
    }

    pub fn page_fetch(
        url: impl Into<String>,
        authorization: WebAuthorization,
    ) -> Result<Self, LinkError> {
        let url = url.into();
        if url.trim().is_empty() {
            return Err(LinkError::InvalidJob("url is empty"));
        }
        Ok(Self {
            job_id: Uuid::new_v4(),
            workflow: WebWorkflow::PageFetch,
            query: None,
            url: Some(url),
            prompt: None,
            authorization,
            limit: 1,
            timeout_ms: 30_000,
            idle_timeout_ms: None,
        })
    }

    pub fn chatgpt(
        prompt: impl Into<String>,
        authorization: WebAuthorization,
    ) -> Result<Self, LinkError> {
        let prompt = prompt.into();
        if prompt.is_empty() {
            return Err(LinkError::InvalidJob("prompt is empty"));
        }
        Ok(Self {
            job_id: Uuid::new_v4(),
            workflow: WebWorkflow::ChatGptWeb,
            query: None,
            url: None,
            prompt: Some(prompt),
            authorization,
            limit: 1,
            timeout_ms: CHATGPT_WEB_HARD_TIMEOUT_MS,
            idle_timeout_ms: Some(CHATGPT_WEB_IDLE_TIMEOUT_MS),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkResult {
    pub workflow: WebWorkflow,
    #[serde(default)]
    pub data: Value,
    #[serde(default = "default_mutation_state")]
    pub mutation_state: MutationState,
    #[serde(default)]
    pub lifecycle: Option<LifecycleState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkProgress {
    pub job_id: Uuid,
    pub workflow: WebWorkflow,
    pub sequence: u64,
    pub delta: String,
    pub replace: bool,
}

impl LinkResult {
    /// Normalizes every supported bridge citation shape into a safe HTTPS URL
    /// for Taceta's transcript UI. ChatGPT Web returns `{ title, url }`
    /// objects, while page-fetch workflows return URL strings.
    pub fn citation_urls(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.data
            .get("citations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|citation| {
                citation
                    .as_str()
                    .or_else(|| citation.get("url").and_then(Value::as_str))
            })
            .filter_map(|raw| {
                let url = url::Url::parse(raw).ok()?;
                if url.scheme() != "https"
                    || url.username() != ""
                    || url.password().is_some()
                    || url.host_str().is_none()
                {
                    return None;
                }
                Some(url.to_string())
            })
            .filter(|url| seen.insert(url.clone()))
            .collect()
    }

    /// Browser output is context for local synthesis, never Taceta's final answer.
    pub fn untrusted_context(&self) -> String {
        serde_json::json!({
            "source":"browser_workflow",
            "trusted":false,
            "workflow":self.workflow,
            "citation_urls":self.citation_urls(),
            "data":self.data,
        })
        .to_string()
    }
}

fn default_mutation_state() -> MutationState {
    MutationState::NotPerformed
}

#[derive(Debug, Error)]
pub enum LinkError {
    #[error("invalid browser job: {0}")]
    InvalidJob(&'static str),
    #[error("browser link is unavailable")]
    Unavailable,
    #[error("browser authentication is required")]
    AuthRequired,
    #[error("browser request timed out")]
    Timeout,
    #[error("browser request outcome is ambiguous; retry explicitly")]
    PerformedOrUnknown,
    #[error("browser link protocol error: {0}")]
    Protocol(String),
    #[error("browser link transport error: {0}")]
    Transport(#[from] ProtocolError),
}

/// App-owned queue and socket handler. The extension polls this queue; the app
/// never assumes that Native Messaging can push into a browser background page.
#[derive(Clone, Default)]
pub struct TacetaLinkService {
    queue: Arc<Mutex<VecDeque<LinkJob>>>,
    /// Jobs inserted by `enqueue_and_wait*` must still have a live receiver
    /// when the extension polls them. Directly enqueued jobs keep their
    /// existing fire-and-forget behavior.
    waiting_queue_jobs: Arc<Mutex<HashSet<Uuid>>>,
    waiters: Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<LinkResult, LinkError>>>>>,
    progress_waiters: Arc<Mutex<HashMap<Uuid, UnboundedSender<LinkProgress>>>>,
    used_authorizations: Arc<Mutex<HashSet<Uuid>>>,
    last_seen: Arc<Mutex<Option<Instant>>>,
}

struct PendingJobRegistration {
    service: TacetaLinkService,
    job_id: Uuid,
    armed: bool,
}

impl PendingJobRegistration {
    fn new(service: TacetaLinkService, job_id: Uuid) -> Self {
        Self {
            service,
            job_id,
            armed: true,
        }
    }

    fn complete(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingJobRegistration {
    fn drop(&mut self) {
        if self.armed {
            self.service.cancel_pending_job(self.job_id);
        }
    }
}

const EXTENSION_HEARTBEAT_TTL: Duration = Duration::from_secs(90);

impl TacetaLinkService {
    /// Returns true only while a protocol-valid extension heartbeat was seen
    /// recently. No browser state or credentials are retained.
    pub fn is_extension_connected(&self) -> bool {
        self.last_seen
            .lock()
            .expect("link heartbeat lock")
            .is_some_and(|seen| seen.elapsed() <= EXTENSION_HEARTBEAT_TTL)
    }

    pub fn enqueue(&self, job: LinkJob) -> Result<(), LinkError> {
        if !self
            .used_authorizations
            .lock()
            .expect("authorization lock")
            .insert(job.authorization.request_id)
        {
            return Err(LinkError::PerformedOrUnknown);
        }
        self.queue.lock().expect("link queue").push_back(job);
        Ok(())
    }
    pub async fn enqueue_and_wait(&self, job: LinkJob) -> Result<LinkResult, LinkError> {
        self.enqueue_and_wait_inner(job, None).await
    }
    pub async fn enqueue_and_wait_with_progress(
        &self,
        job: LinkJob,
        progress: UnboundedSender<LinkProgress>,
    ) -> Result<LinkResult, LinkError> {
        self.enqueue_and_wait_inner(job, Some(progress)).await
    }
    async fn enqueue_and_wait_inner(
        &self,
        job: LinkJob,
        progress: Option<UnboundedSender<LinkProgress>>,
    ) -> Result<LinkResult, LinkError> {
        let job_id = job.job_id;
        let (sender, receiver) = oneshot::channel();
        self.waiters
            .lock()
            .expect("link waiters")
            .insert(job_id, sender);
        if let Some(progress) = progress {
            self.progress_waiters
                .lock()
                .expect("link progress waiters")
                .insert(job_id, progress);
        }
        self.waiting_queue_jobs
            .lock()
            .expect("waiting link jobs")
            .insert(job_id);
        if let Err(error) = self.enqueue(job) {
            self.waiting_queue_jobs
                .lock()
                .expect("waiting link jobs")
                .remove(&job_id);
            self.waiters.lock().expect("link waiters").remove(&job_id);
            self.progress_waiters
                .lock()
                .expect("link progress waiters")
                .remove(&job_id);
            return Err(error);
        }
        let registration = PendingJobRegistration::new(self.clone(), job_id);
        let outcome = receiver.await.map_err(|_| LinkError::Unavailable)?;
        registration.complete();
        outcome
    }

    fn cancel_pending_job(&self, job_id: Uuid) {
        self.queue
            .lock()
            .expect("link queue")
            .retain(|job| job.job_id != job_id);
        self.waiting_queue_jobs
            .lock()
            .expect("waiting link jobs")
            .remove(&job_id);
        self.waiters.lock().expect("link waiters").remove(&job_id);
        self.progress_waiters
            .lock()
            .expect("link progress waiters")
            .remove(&job_id);
    }

    fn pop_next_live_job(&self) -> Option<LinkJob> {
        loop {
            let job = self.queue.lock().expect("link queue").pop_front()?;
            let requires_live_waiter = self
                .waiting_queue_jobs
                .lock()
                .expect("waiting link jobs")
                .remove(&job.job_id);
            if !requires_live_waiter {
                return Some(job);
            }
            let live = self
                .waiters
                .lock()
                .expect("link waiters")
                .get(&job.job_id)
                .is_some_and(|waiter| !waiter.is_closed());
            if live {
                return Some(job);
            }
            self.cancel_pending_job(job.job_id);
        }
    }
    #[cfg(unix)]
    pub fn serve_connection(
        &self,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<(), ProtocolError> {
        let mut input = BufReader::new(stream.try_clone()?);
        let mut output = BufWriter::new(stream);
        let request: Envelope<Value> = crate::browser_harness::read_frame(&mut input)?;
        validate_service_request(&request)?;
        if matches!(
            request.operation,
            Operation::Ping | Operation::Health | Operation::ExtensionReady | Operation::PollJob
        ) {
            *self.last_seen.lock().expect("link heartbeat lock") = Some(Instant::now());
        }
        let response = match request.operation {
            Operation::Ping | Operation::Health | Operation::ExtensionReady => {
                Envelope::response_for(
                    &request,
                    request.operation.clone(),
                    serde_json::json!({"status":"ready"}),
                )
            }
            Operation::PollJob => {
                let job = self.pop_next_live_job();
                let payload = match job {
                    // The extension scopes a job authorization to the Native
                    // Messaging connection that polled it.  The app creates
                    // the one-shot request authorization before that
                    // connection exists, so bind the wire authorization to
                    // this poller's session at the protocol boundary.
                    Some(job) => serde_json::json!({"job": wire_job(&job, request.session_id)}),
                    None => serde_json::json!({"job":null}),
                };
                Envelope::response_for(&request, Operation::PollJob, payload)
            }
            Operation::JobProgress => {
                let progress = normalize_progress(request.payload.clone());
                let accepted = progress.is_ok_and(|progress| {
                    self.progress_waiters
                        .lock()
                        .expect("link progress waiters")
                        .get(&progress.job_id)
                        .is_some_and(|waiter| waiter.send(progress).is_ok())
                });
                Envelope::response_for(
                    &request,
                    Operation::JobProgress,
                    serde_json::json!({"accepted":accepted}),
                )
            }
            Operation::JobResult => {
                let result = normalize_result(request.payload.clone());
                if let Some(id) = request
                    .payload
                    .get("job_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    self.waiting_queue_jobs
                        .lock()
                        .expect("waiting link jobs")
                        .remove(&id);
                    self.progress_waiters
                        .lock()
                        .expect("link progress waiters")
                        .remove(&id);
                    if let Some(waiter) = self.waiters.lock().expect("link waiters").remove(&id) {
                        let _ = waiter.send(result);
                    }
                }
                Envelope::response_for(
                    &request,
                    Operation::JobResult,
                    serde_json::json!({"accepted":true}),
                )
            }
            Operation::Cancel => Envelope::response_for(
                &request,
                Operation::CancelAck,
                serde_json::json!({"cancelled":true}),
            ),
            _ => Envelope::response_for(
                &request,
                Operation::VersionFailure,
                serde_json::json!({"error":"operation_not_allowed"}),
            ),
        };
        crate::browser_harness::write_frame(&mut output, &response)
    }
}

fn validate_service_request(request: &Envelope<Value>) -> Result<(), ProtocolError> {
    crate::browser_harness::validate_envelope(request, env!("CARGO_PKG_VERSION"))?;
    if request.message_type != crate::browser_harness::MessageType::Request {
        return Err(ProtocolError::CorrelationMismatch);
    }
    Ok(())
}

fn wire_job(job: &LinkJob, session_id: Uuid) -> Value {
    let workflow = match job.workflow {
        WebWorkflow::DefaultSearch => "default_search",
        WebWorkflow::GoogleSearch => "google_search",
        WebWorkflow::PageFetch => "page_fetch",
        WebWorkflow::ChatGptWeb => "chatgpt_web",
    };
    serde_json::json!({"job_id":job.job_id,"workflow":workflow,"query":job.query,"url":job.url,"prompt":job.prompt,"limit":job.limit,"timeout_ms":job.timeout_ms,"idle_timeout_ms":job.idle_timeout_ms,
        "authorization":{"kind":"web_request","request_id":job.authorization.request_id,"session_id":session_id,"once":true}})
}

fn normalize_result(payload: Value) -> Result<LinkResult, LinkError> {
    let workflow_name = payload
        .get("workflow")
        .and_then(Value::as_str)
        .ok_or_else(|| LinkError::Protocol("missing workflow".into()))?;
    let workflow = match workflow_name {
        "default_search" => WebWorkflow::DefaultSearch,
        "google_search" => WebWorkflow::GoogleSearch,
        "page_fetch" => WebWorkflow::PageFetch,
        "chatgpt_web" => WebWorkflow::ChatGptWeb,
        other => return Err(LinkError::Protocol(format!("unknown workflow: {other}"))),
    };
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| LinkError::Protocol("missing job result status".into()))?;
    if status == "failed" {
        let error = payload
            .get("error")
            .and_then(Value::as_object)
            .ok_or_else(|| LinkError::Protocol("missing job result error".into()))?;
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("workflow_failed");
        let message = error.get("message").and_then(Value::as_str).unwrap_or(code);
        return Err(match code {
            "auth_required" => LinkError::AuthRequired,
            "timeout" | "search_timeout" | "composer_timeout" | "response_timeout" => {
                LinkError::Protocol(format!("Taceta Link {workflow_name} request timed out"))
            }
            "response_stalled" => LinkError::Protocol(format!(
                "Taceta Link {workflow_name} response progress stalled"
            )),
            "response_hard_timeout" => LinkError::Protocol(format!(
                "Taceta Link {workflow_name} exceeded the safety time limit"
            )),
            "performed_or_unknown" => LinkError::PerformedOrUnknown,
            _ => LinkError::Protocol(format!("{workflow_name} {code}: {message}")),
        });
    }
    if status != "completed" {
        return Err(LinkError::Protocol(format!(
            "unknown job result status: {status}"
        )));
    }
    let mutation_state: MutationState = serde_json::from_value(
        payload
            .get("mutation_state")
            .cloned()
            .ok_or_else(|| LinkError::Protocol("missing mutation state".into()))?,
    )
    .map_err(|error| LinkError::Protocol(error.to_string()))?;
    if mutation_state != MutationState::Performed {
        return Err(LinkError::PerformedOrUnknown);
    }
    Ok(LinkResult {
        workflow,
        data: payload,
        mutation_state,
        lifecycle: Some(LifecycleState::Completed),
    })
}

fn normalize_progress(payload: Value) -> Result<LinkProgress, LinkError> {
    let job_id = payload
        .get("job_id")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| LinkError::Protocol("missing progress job ID".into()))?;
    let workflow = match payload.get("workflow").and_then(Value::as_str) {
        Some("chatgpt_web") => WebWorkflow::ChatGptWeb,
        Some(other) => {
            return Err(LinkError::Protocol(format!(
                "progress is not supported for {other}"
            )));
        }
        None => return Err(LinkError::Protocol("missing progress workflow".into())),
    };
    let sequence = payload
        .get("sequence")
        .and_then(Value::as_u64)
        .filter(|sequence| *sequence > 0)
        .ok_or_else(|| LinkError::Protocol("invalid progress sequence".into()))?;
    let delta = payload
        .get("delta")
        .and_then(Value::as_str)
        .filter(|delta| !delta.is_empty())
        .ok_or_else(|| LinkError::Protocol("empty progress delta".into()))?
        .to_owned();
    let replace = payload
        .get("replace")
        .and_then(Value::as_bool)
        .ok_or_else(|| LinkError::Protocol("missing progress replacement state".into()))?;
    if payload.get("status").and_then(Value::as_str) != Some("streaming")
        || payload.get("mutation_state").and_then(Value::as_str) != Some("performed")
    {
        return Err(LinkError::Protocol("invalid progress state".into()));
    }
    Ok(LinkProgress {
        job_id,
        workflow,
        sequence,
        delta,
        replace,
    })
}

/// Build one ChatGPT Web job for a user-authorized Web turn. A direct search
/// preserves the current input exactly. When the local model first creates the
/// concrete research question, the prompt keeps the current input as authority
/// and also carries that model query. Explicitly enabled follow-ups use the
/// same anchored envelope.
pub fn chatgpt_job_for_turn(
    current_input: &ChatMessage,
    query: &str,
    request_ordinal: u8,
    prompt_source: ChatGptPromptSource,
    authorization: WebAuthorization,
) -> Result<LinkJob, LinkError> {
    if current_input.role != Role::User {
        return Err(LinkError::InvalidJob(
            "current input must be a user message",
        ));
    }
    if !(1..=3).contains(&request_ordinal) {
        return Err(LinkError::InvalidJob(
            "ChatGPT request ordinal must be between 1 and 3",
        ));
    }
    let prompt = match (request_ordinal, prompt_source) {
        (1, ChatGptPromptSource::CurrentUserInput) => current_input.content.clone(),
        (1, ChatGptPromptSource::LocalModelQuery) => format!(
            "Original user request:\n{}\n\nConcrete research question proposed by the local model:\n{}\n\nResearch the concrete question. Treat names, versions, and premises in the local-model question as unverified and correct them when necessary. Return the findings needed to answer that question.",
            current_input.content, query
        ),
        _ => format!(
            "Original user request:\n{}\n\nAdditional research angle proposed by the local model:\n{}\n\nResearch the original request from this additional angle. Treat names, versions, and premises in the additional angle as unverified and correct them when necessary. Return findings relevant to the original request.",
            current_input.content, query
        ),
    };
    LinkJob::chatgpt(prompt, authorization)
}

pub fn job_for_workflow(
    workflow: WebWorkflow,
    query: Option<&str>,
    current_input: Option<&ChatMessage>,
    chatgpt_request_ordinal: u8,
    chatgpt_prompt_source: ChatGptPromptSource,
    authorization: WebAuthorization,
) -> Result<LinkJob, LinkError> {
    let job_authorization = fresh_job_authorization(&authorization);
    match workflow {
        WebWorkflow::DefaultSearch | WebWorkflow::GoogleSearch => LinkJob::search(
            workflow,
            query.ok_or(LinkError::InvalidJob("query is required"))?,
            job_authorization,
        ),
        WebWorkflow::PageFetch => Err(LinkError::InvalidJob("page fetch requires a URL")),
        WebWorkflow::ChatGptWeb => chatgpt_job_for_turn(
            current_input.ok_or(LinkError::InvalidJob("current user input is required"))?,
            query.ok_or(LinkError::InvalidJob("query is required"))?,
            chatgpt_request_ordinal,
            chatgpt_prompt_source,
            job_authorization,
        ),
    }
}

/// Builds one browser page-read job under an already user-authorized Web: ON
/// turn.  Each job gets an independent replay nonce while retaining the turn
/// session ID for app-side correlation.
pub fn page_fetch_job(
    url: impl Into<String>,
    authorization: WebAuthorization,
) -> Result<LinkJob, LinkError> {
    LinkJob::page_fetch(url, fresh_job_authorization(&authorization))
}

fn fresh_job_authorization(turn_authorization: &WebAuthorization) -> WebAuthorization {
    WebAuthorization {
        request_id: Uuid::new_v4(),
        session_id: turn_authorization.session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_harness::ConnectionManager;
    #[test]
    fn jobs_keep_query_url_and_prompt_exclusive() {
        let auth = WebAuthorization {
            request_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
        };
        let job = LinkJob::search(WebWorkflow::GoogleSearch, "rust", auth.clone()).unwrap();
        assert!(job.query.is_some());
        assert!(job.url.is_none());
        assert!(job.prompt.is_none());
        let page = LinkJob::page_fetch("https://example.com", auth.clone()).unwrap();
        assert!(page.query.is_none());
        assert!(page.url.is_some());
        assert!(page.prompt.is_none());
        assert!(matches!(
            LinkJob::search(WebWorkflow::ChatGptWeb, "bad", auth),
            Err(LinkError::InvalidJob(_))
        ));
    }
    #[test]
    fn chatgpt_web_first_prompt_preserves_direct_input_and_anchors_generated_question() {
        let mut input = ChatMessage::new_user("  exact\ninput  ");
        input.thinking = "private trace".into();
        input.attachments.push(crate::domain::Attachment {
            name: "private.txt".into(),
            payload: crate::domain::AttachmentPayload::Text("private attachment".into()),
        });
        let authorization = WebAuthorization {
            request_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
        };
        let first = chatgpt_job_for_turn(
            &input,
            "incorrect model/version premise",
            1,
            ChatGptPromptSource::CurrentUserInput,
            authorization.clone(),
        )
        .unwrap();
        assert_eq!(first.prompt.as_deref(), Some("  exact\ninput  "));

        let planned = chatgpt_job_for_turn(
            &input,
            "Which new model should be researched?",
            1,
            ChatGptPromptSource::LocalModelQuery,
            authorization.clone(),
        )
        .unwrap();
        let planned_prompt = planned.prompt.unwrap();
        assert!(planned_prompt.contains("  exact\ninput  "));
        assert!(planned_prompt.contains("Which new model should be researched?"));
        assert!(planned_prompt.contains("Research the concrete question"));
        assert!(!planned_prompt.contains("private trace"));
        assert!(!planned_prompt.contains("private attachment"));

        let followup = chatgpt_job_for_turn(
            &input,
            "incorrect model/version premise",
            2,
            ChatGptPromptSource::LocalModelQuery,
            authorization,
        )
        .unwrap();
        let prompt = followup.prompt.unwrap();
        assert!(prompt.contains("  exact\ninput  "));
        assert!(prompt.contains("incorrect model/version premise"));
        assert!(prompt.contains("unverified and correct them when necessary"));
        assert!(!prompt.contains("private trace"));
        assert!(!prompt.contains("private attachment"));
        assert!(followup.query.is_none());
        assert_eq!(followup.timeout_ms, CHATGPT_WEB_HARD_TIMEOUT_MS);
        assert_eq!(followup.idle_timeout_ms, Some(CHATGPT_WEB_IDLE_TIMEOUT_MS));
    }

    #[test]
    fn chatgpt_web_rejects_a_request_outside_the_explicit_one_to_three_limit() {
        let input = ChatMessage::new_user("exact input");
        let result = chatgpt_job_for_turn(
            &input,
            "additional angle",
            4,
            ChatGptPromptSource::CurrentUserInput,
            WebAuthorization {
                request_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
            },
        );
        assert!(matches!(result, Err(LinkError::InvalidJob(_))));
    }

    #[test]
    fn authorization_is_one_shot_even_when_transport_is_unavailable() {
        let auth = WebAuthorization {
            request_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
        };
        let job = LinkJob::search(WebWorkflow::GoogleSearch, "rust", auth).unwrap();
        let service = TacetaLinkService::default();
        assert!(service.enqueue(job.clone()).is_ok());
        assert!(matches!(
            service.enqueue(job),
            Err(LinkError::PerformedOrUnknown)
        ));
    }

    #[tokio::test]
    async fn dropping_a_pending_wait_removes_its_job_and_receivers() {
        let service = TacetaLinkService::default();
        let authorization = WebAuthorization {
            request_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
        };
        let job = LinkJob::search(WebWorkflow::GoogleSearch, "stale", authorization).unwrap();
        let job_id = job.job_id;
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let pending = service.enqueue_and_wait_with_progress(job, progress_tx);
            tokio::pin!(pending);
            assert!(futures_util::poll!(&mut pending).is_pending());
            assert_eq!(service.queue.lock().unwrap().len(), 1);
            assert!(service.waiters.lock().unwrap().contains_key(&job_id));
            assert!(
                service
                    .progress_waiters
                    .lock()
                    .unwrap()
                    .contains_key(&job_id)
            );
        }

        assert!(service.queue.lock().unwrap().is_empty());
        assert!(service.waiting_queue_jobs.lock().unwrap().is_empty());
        assert!(!service.waiters.lock().unwrap().contains_key(&job_id));
        assert!(
            !service
                .progress_waiters
                .lock()
                .unwrap()
                .contains_key(&job_id)
        );
        assert!(service.pop_next_live_job().is_none());
    }

    #[test]
    fn each_job_in_one_web_turn_has_a_fresh_replay_nonce() {
        let turn = WebAuthorization {
            request_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
        };
        let search = job_for_workflow(
            WebWorkflow::GoogleSearch,
            Some("latest Rust release"),
            None,
            1,
            ChatGptPromptSource::CurrentUserInput,
            turn.clone(),
        )
        .unwrap();
        let page = page_fetch_job("https://example.com/releases", turn.clone()).unwrap();
        assert_ne!(
            search.authorization.request_id,
            page.authorization.request_id
        );
        assert_eq!(search.authorization.session_id, turn.session_id);
        assert_eq!(page.authorization.session_id, turn.session_id);

        let service = TacetaLinkService::default();
        assert!(service.enqueue(search).is_ok());
        assert!(service.enqueue(page).is_ok());
    }

    #[test]
    fn heartbeat_starts_disconnected_and_valid_request_marks_connected() {
        let service = TacetaLinkService::default();
        assert!(!service.is_extension_connected());
        let path = default_socket_path().unwrap().with_file_name("health.sock");
        let server = crate::browser_harness::SocketServer::bind(&path).unwrap();
        let service_for_server = service.clone();
        let session = Uuid::new_v4();
        let worker = std::thread::spawn(move || {
            let stream = server.accept().unwrap();
            service_for_server.serve_connection(stream).unwrap();
        });
        let mut client = ConnectionManager::connect(&path, "0.1.0", session).unwrap();
        let request = Envelope::new("0.1.0", session, Operation::Health, serde_json::json!({}));
        client.send(&request).unwrap();
        let _: Envelope<Value> = client.receive().unwrap();
        worker.join().unwrap();
        assert!(service.is_extension_connected());
    }

    #[test]
    fn invalid_protocol_request_does_not_mark_connected() {
        let service = TacetaLinkService::default();
        let path = default_socket_path()
            .unwrap()
            .with_file_name("invalid-health.sock");
        let server = crate::browser_harness::SocketServer::bind(&path).unwrap();
        let service_for_server = service.clone();
        let session = Uuid::new_v4();
        let worker = std::thread::spawn(move || {
            let stream = server.accept().unwrap();
            assert!(service_for_server.serve_connection(stream).is_err());
        });
        let mut client = ConnectionManager::connect(&path, "wrong-version", session).unwrap();
        let request = Envelope::new(
            "wrong-version",
            session,
            Operation::Health,
            serde_json::json!({}),
        );
        client.send(&request).unwrap();
        worker.join().unwrap();
        assert!(!service.is_extension_connected());
    }

    #[test]
    fn browser_result_is_explicitly_untrusted_context() {
        let result = LinkResult {
            workflow: WebWorkflow::GoogleSearch,
            data: serde_json::json!({"answer":"untrusted", "citations":["https://example.com"]}),
            mutation_state: MutationState::Performed,
            lifecycle: Some(LifecycleState::Completed),
        };
        let context = result.untrusted_context();
        assert!(context.contains("\"trusted\":false"));
        assert!(context.contains("untrusted"));
    }

    #[test]
    fn chatgpt_citation_objects_reach_context_and_taceta_as_https_urls() {
        let result = LinkResult {
            workflow: WebWorkflow::ChatGptWeb,
            data: serde_json::json!({
                "answer": "sourced answer",
                "citations": [
                    {"title": "Example", "url": "https://example.com/source"},
                    "https://www.rust-lang.org/learn",
                    {"title": "Duplicate", "url": "https://example.com/source"},
                    {"title": "Unsafe", "url": "javascript:alert(1)"}
                ]
            }),
            mutation_state: MutationState::Performed,
            lifecycle: Some(LifecycleState::Completed),
        };

        assert_eq!(
            result.citation_urls(),
            vec![
                "https://example.com/source".to_owned(),
                "https://www.rust-lang.org/learn".to_owned(),
            ]
        );
        let context: Value = serde_json::from_str(&result.untrusted_context()).unwrap();
        assert_eq!(context["citation_urls"][0], "https://example.com/source");
        assert_eq!(context["data"]["citations"][0]["title"], "Example");
    }

    #[test]
    fn extension_result_shape_preserves_workflow_and_mutation_state() {
        let result = normalize_result(serde_json::json!({
            "job_id": Uuid::new_v4(),
            "workflow": "google_search",
            "status": "completed",
            "results": {"provider":"google","query":"rust","results":[]},
            "citations": [],
            "mutation_state": "performed"
        }))
        .unwrap();

        assert_eq!(result.workflow, WebWorkflow::GoogleSearch);
        assert_eq!(result.mutation_state, MutationState::Performed);
    }

    #[test]
    fn extension_page_fetch_result_is_accepted() {
        let result = normalize_result(serde_json::json!({
            "job_id": Uuid::new_v4(),
            "workflow": "page_fetch",
            "status": "completed",
            "page": {"url":"https://example.com","title":"Example","text":"body"},
            "citations": ["https://example.com"],
            "mutation_state": "performed"
        }))
        .unwrap();

        assert_eq!(result.workflow, WebWorkflow::PageFetch);
        assert_eq!(result.data["citations"][0], "https://example.com");
    }

    #[test]
    fn extension_failure_keeps_workflow_in_timeout_error() {
        let result = normalize_result(serde_json::json!({
            "job_id": Uuid::new_v4(),
            "workflow": "google_search",
            "status": "failed",
            "error": {"code":"search_timeout","message":"search_timeout"},
            "mutation_state": "performed_or_unknown"
        }));

        assert!(matches!(
            result,
            Err(LinkError::Protocol(message))
                if message == "Taceta Link google_search request timed out"
        ));
    }

    #[test]
    fn chatgpt_progress_carries_ordered_text_delta() {
        let job_id = Uuid::new_v4();
        let progress = normalize_progress(serde_json::json!({
            "job_id": job_id,
            "workflow": "chatgpt_web",
            "sequence": 2,
            "delta": "続き",
            "replace": false,
            "status": "streaming",
            "mutation_state": "performed"
        }))
        .unwrap();
        assert_eq!(progress.job_id, job_id);
        assert_eq!(progress.sequence, 2);
        assert_eq!(progress.delta, "続き");
        assert!(!progress.replace);
    }

    #[cfg(unix)]
    #[test]
    fn socket_forwards_chatgpt_progress_before_final_result() {
        let path = default_socket_path()
            .unwrap()
            .with_file_name("service-progress.sock");
        let server = crate::browser_harness::SocketServer::bind(&path).unwrap();
        let service = TacetaLinkService::default();
        let job_id = Uuid::new_v4();
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        service
            .progress_waiters
            .lock()
            .unwrap()
            .insert(job_id, progress_tx);
        let service_for_worker = service.clone();
        let worker = std::thread::spawn(move || {
            let stream = server.accept().unwrap();
            service_for_worker.serve_connection(stream).unwrap();
        });
        let session = Uuid::new_v4();
        let mut client = ConnectionManager::connect(&path, "0.1.0", session).unwrap();
        let request = Envelope::new(
            "0.1.0",
            session,
            Operation::JobProgress,
            serde_json::json!({
                "job_id": job_id,
                "workflow": "chatgpt_web",
                "sequence": 1,
                "delta": "回答",
                "replace": false,
                "status": "streaming",
                "mutation_state": "performed"
            }),
        );
        client.send(&request).unwrap();
        let ack: Envelope<Value> = client.receive().unwrap();
        assert_eq!(ack.operation, Operation::JobProgress);
        assert_eq!(ack.payload["accepted"], true);
        let progress = progress_rx.try_recv().unwrap();
        assert_eq!(progress.delta, "回答");
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn socket_poll_job_result_roundtrip() {
        let path = default_socket_path()
            .unwrap()
            .with_file_name("service-roundtrip.sock");
        let server = crate::browser_harness::SocketServer::bind(&path).unwrap();
        let session = Uuid::new_v4();
        let auth = WebAuthorization {
            request_id: Uuid::new_v4(),
            // App-side authorization is intentionally created before the
            // extension polls; the service must rebind it to `session`.
            session_id: Uuid::new_v4(),
        };
        let job = LinkJob::search(WebWorkflow::GoogleSearch, "rust", auth).unwrap();
        let service = TacetaLinkService::default();
        service.enqueue(job.clone()).unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..2 {
                let stream = server.accept().unwrap();
                service.serve_connection(stream).unwrap();
            }
        });
        let mut client = ConnectionManager::connect(&path, "0.1.0", session).unwrap();
        let poll = Envelope::new("0.1.0", session, Operation::PollJob, serde_json::json!({}));
        client.send(&poll).unwrap();
        let response: Envelope<Value> = client.receive().unwrap();
        assert_eq!(response.operation, Operation::PollJob);
        assert_eq!(response.payload["job"]["job_id"], job.job_id.to_string());
        assert_eq!(response.payload["job"]["workflow"], "google_search");
        assert_eq!(
            response.payload["job"]["authorization"]["request_id"],
            job.authorization.request_id.to_string()
        );
        assert_eq!(
            response.payload["job"]["authorization"]["session_id"],
            session.to_string()
        );
        assert_ne!(
            response.payload["job"]["authorization"]["session_id"],
            job.authorization.session_id.to_string()
        );
        let mut result_client = ConnectionManager::connect(&path, "0.1.0", session).unwrap();
        let result_request = Envelope::new(
            "0.1.0",
            session,
            Operation::JobResult,
            serde_json::json!({"job_id":job.job_id,"status":"completed","answer":"untrusted","citations":[]}),
        );
        result_client.send(&result_request).unwrap();
        let ack: Envelope<Value> = result_client.receive().unwrap();
        assert_eq!(ack.operation, Operation::JobResult);
        assert_eq!(ack.payload["accepted"], true);
        worker.join().unwrap();
    }
}
