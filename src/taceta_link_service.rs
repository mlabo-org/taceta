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
use tokio::sync::oneshot;
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

impl LinkResult {
    /// Browser output is context for local synthesis, never Taceta's final answer.
    pub fn untrusted_context(&self) -> String {
        serde_json::json!({"source":"browser_workflow", "trusted":false, "workflow":self.workflow, "data":self.data}).to_string()
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
    waiters: Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<LinkResult, LinkError>>>>>,
    used_authorizations: Arc<Mutex<HashSet<Uuid>>>,
    last_seen: Arc<Mutex<Option<Instant>>>,
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
        let job_id = job.job_id;
        let (sender, receiver) = oneshot::channel();
        self.waiters
            .lock()
            .expect("link waiters")
            .insert(job_id, sender);
        if let Err(error) = self.enqueue(job) {
            self.waiters.lock().expect("link waiters").remove(&job_id);
            return Err(error);
        }
        receiver.await.map_err(|_| LinkError::Unavailable)?
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
                let job = self.queue.lock().expect("link queue").pop_front();
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
            Operation::JobResult => {
                let result = normalize_result(request.payload.clone());
                if let Some(id) = request
                    .payload
                    .get("job_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
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

/// Build the ChatGPT Web job from the current input only. History, system
/// messages, thinking traces, and attachments are intentionally ignored.
pub fn chatgpt_job_from_current_input(
    message: &ChatMessage,
    authorization: WebAuthorization,
) -> Result<LinkJob, LinkError> {
    if message.role != Role::User {
        return Err(LinkError::InvalidJob(
            "current input must be a user message",
        ));
    }
    LinkJob::chatgpt(message.content.clone(), authorization)
}

pub fn job_for_workflow(
    workflow: WebWorkflow,
    query: Option<&str>,
    current_input: &ChatMessage,
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
        WebWorkflow::ChatGptWeb => chatgpt_job_from_current_input(current_input, job_authorization),
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
    fn chat_prompt_is_exact_and_ignores_history_fields() {
        let mut message = ChatMessage::new_user("  exact\ninput  ");
        message.thinking = "secret trace".into();
        message.attachments.push(crate::domain::Attachment {
            name: "x".into(),
            payload: crate::domain::AttachmentPayload::Text("ignored".into()),
        });
        let job = chatgpt_job_from_current_input(
            &message,
            WebAuthorization {
                request_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
            },
        )
        .unwrap();
        assert_eq!(job.prompt.as_deref(), Some("  exact\ninput  "));
        assert!(job.query.is_none());
        assert_eq!(job.timeout_ms, CHATGPT_WEB_HARD_TIMEOUT_MS);
        assert_eq!(job.idle_timeout_ms, Some(CHATGPT_WEB_IDLE_TIMEOUT_MS));
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

    #[test]
    fn each_job_in_one_web_turn_has_a_fresh_replay_nonce() {
        let turn = WebAuthorization {
            request_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
        };
        let input = ChatMessage::new_user("latest Rust release");
        let search = job_for_workflow(
            WebWorkflow::GoogleSearch,
            Some("latest Rust release"),
            &input,
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
