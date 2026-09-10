use super::*;
use crate::{
    agent::{AgentMessage, AgentRole, AgentToolCall, ToolDefinition},
    domain::{Attachment, ChatMessage, ThinkingCapability, ThinkingLevel, ThinkingMode},
};
use auth::{CredentialStore, Credentials};
use bytes::Bytes;
use std::sync::{Mutex, atomic::AtomicUsize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

#[derive(Default)]
pub(super) struct MemoryStore {
    bytes: Mutex<Option<Vec<u8>>>,
    pub(super) reads: AtomicUsize,
    pub(super) saves: AtomicUsize,
    pub(super) deletes: AtomicUsize,
}

impl CredentialStore for MemoryStore {
    fn load(&self) -> Result<Option<Credentials>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .lock()
            .unwrap()
            .as_ref()
            .map(|bytes| {
                serde_json::from_slice(bytes).map_err(|_| "fixture credential invalid".into())
            })
            .transpose()
    }
    fn save(&self, token: &Credentials) -> Result<(), String> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        *self.bytes.lock().unwrap() = Some(serde_json::to_vec(token).unwrap());
        Ok(())
    }
    fn delete(&self) -> Result<(), String> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        *self.bytes.lock().unwrap() = None;
        Ok(())
    }
}

pub(super) fn saved_token(store: &MemoryStore, expired: bool) {
    store
        .save(&Credentials {
            access_token: "fixture-access-old".into(),
            refresh_token: Some("fixture-refresh-old".into()),
            expires_at: Some(if expired { 0 } else { auth::now() + 3600 }),
        })
        .unwrap();
}

pub(super) struct RecordedRequest {
    pub(super) path: String,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Vec<u8>,
}
pub(super) struct Reply {
    pub(super) status: &'static str,
    pub(super) content_type: &'static str,
    pub(super) body: Vec<u8>,
}
impl Reply {
    pub(super) fn json(value: Value) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: serde_json::to_vec(&value).unwrap(),
        }
    }
}

pub(super) struct Fixture {
    pub(super) base: String,
    pub(super) requests: mpsc::UnboundedReceiver<RecordedRequest>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    pub(super) async fn start(replies: Vec<Reply>) -> Self {
        Self::start_with_required_version(replies, None).await
    }

    async fn start_with_required_version(
        replies: Vec<Reply>,
        required_version: Option<&'static str>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let end = loop {
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0, "fixture request closed before headers");
                    bytes.extend_from_slice(&chunk[..size]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header_text = std::str::from_utf8(&bytes[..end]).unwrap();
                let path = header_text
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let headers: HashMap<_, _> = header_text
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let length = headers
                    .get("content-length")
                    .map(|value| value.parse::<usize>().unwrap())
                    .unwrap_or(0);
                while bytes.len() - end < length {
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0, "fixture request closed before body");
                    bytes.extend_from_slice(&chunk[..size]);
                }
                let reply = if matches!(path.as_str(), "/v1/responses" | "/v1/chat/completions")
                    && required_version.is_some_and(|version| {
                        headers.get("x-grok-client-version").map(String::as_str) != Some(version)
                    }) {
                    Reply {
                        status: "426 Upgrade Required",
                        content_type: "application/json",
                        body: br#"{"error":{"code":"unsupported_client_version"}}"#.to_vec(),
                    }
                } else {
                    reply
                };
                tx.send(RecordedRequest {
                    path,
                    headers,
                    body: bytes[end..end + length].to_vec(),
                })
                .unwrap();
                let headers = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.content_type,
                    reply.body.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&reply.body).await.unwrap();
            }
        });
        Self {
            base,
            requests,
            task,
        }
    }
}

fn message(role: AgentRole, content: &str) -> AgentMessage {
    AgentMessage {
        role,
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_name: None,
    }
}

fn event(value: Value) -> Vec<u8> {
    format!("data: {value}\r\n\r\n").into_bytes()
}
fn completed(content: &str, calls: Vec<Value>) -> Value {
    let mut output = vec![
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": content}]}),
    ];
    output.extend(calls);
    json!({"type": "response.completed", "response": {"status": "completed", "output": output, "usage": {"input_tokens": 30, "output_tokens": 9}}})
}

#[test]
fn metadata_uses_advertised_protocol_and_thinking_options() {
    let models = parse_models(&json!({"data": [
        {"id": "future-code", "apiBackend": "responses", "contextWindow": 200000, "supportsReasoningEffort": true,
            "reasoningEfforts": ["low", {"value": "medium", "id": "balanced"}, "high", "xhigh"], "maxCompletionTokens": 512},
        {"modelId": "partial", "api_backend": "responses", "_meta": {"supportsReasoningEffort": true, "reasoningEfforts": ["high"], "totalContextTokens": 60000}, "supportsTools": false},
        {"id": "unverified", "apiBackend": "responses", "supportsReasoningEffort": true},
        {"id": "other-wire", "apiBackend": "messages"}, {"id": "missing-wire"},
        {"id": "hidden", "apiBackend": "responses", "hidden": true}
    ]})).unwrap();
    assert_eq!(models.len(), 4);
    assert_eq!(models[0].descriptor.name, "future-code");
    assert_eq!(models[0].descriptor.context_length, Some(200000));
    assert_eq!(models[0].descriptor.thinking, ThinkingCapability::Levels);
    assert!(models[0].descriptor.tools);
    assert!(!models[0].descriptor.vision);
    assert_eq!(models[0].max_output(8192), 512);
    assert_eq!(
        models[0]
            .reasoning(ThinkingMode::Level(ThinkingLevel::Medium))
            .unwrap(),
        Some(json!({"effort": "medium"}))
    );
    assert!(models[0].reasoning(ThinkingMode::Off).is_err());
    assert_eq!(
        models[1].descriptor.thinking,
        ThinkingCapability::Unverified
    );
    assert!(!models[1].descriptor.tools);
    assert_eq!(models[1].descriptor.context_length, Some(60000));
    assert!(
        models[2]
            .reasoning(ThinkingMode::Level(ThinkingLevel::Low))
            .is_err()
    );
    assert!(
        models[2]
            .reasoning(ThinkingMode::Default)
            .unwrap()
            .is_none()
    );
    assert_eq!(models[3].api, ApiKind::ChatCompletions);
}

#[tokio::test]
async fn split_sse_produces_complete_calls_then_serializes_paired_results() {
    let call_a = json!({"type": "function_call", "id": "item-a", "call_id": "call-a", "name": "read_file", "arguments": "{\"path\":\"日本.txt\"}"});
    let call_b = json!({"type": "function_call", "id": "item-b", "call_id": "call-b", "name": "run_command", "arguments": "{\"command\":\"pwd\"}"});
    let mut bytes = b": keepalive\r\n\r\n".to_vec();
    for value in [
        json!({"type": "response.reasoning_summary_text.delta", "delta": "秘密の思考"}),
        json!({"type": "response.output_text.delta", "delta": "確認"}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "item-a", "call_id": "call-a", "name": "read_file", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "item-b", "call_id": "call-b", "name": "run_command", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "item_id": "item-a", "delta": "{\"path\":\"日"}),
        json!({"type": "response.function_call_arguments.delta", "item_id": "item-b", "delta": "{\"command\":\"pwd\"}"}),
        json!({"type": "response.function_call_arguments.delta", "item_id": "item-a", "delta": "本.txt\"}"}),
        json!({"type": "response.function_call_arguments.done", "item_id": "item-a", "arguments": "{\"path\":\"日本.txt\"}"}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": call_b.clone()}),
        completed("確認します", vec![call_a, call_b]),
    ] {
        bytes.extend(event(value));
    }
    let chunks = bytes
        .chunks(1)
        .map(|chunk| Ok::<_, ()>(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    let mut visible = String::new();
    let mut thinking = String::new();
    let result = responses::consume(futures_util::stream::iter(chunks), |delta| {
        match delta {
            OutputDelta::Content(text) => visible.push_str(&text),
            OutputDelta::Thinking(text) => thinking.push_str(&text),
        }
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(visible, "確認します");
    assert_eq!(thinking, "秘密の思考");
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(result.tool_calls[0].arguments, json!({"path": "日本.txt"}));
    assert_eq!(result.prompt_tokens, Some(30));
    let mut assistant = message(AgentRole::Assistant, &result.content);
    assistant.tool_calls = result.tool_calls;
    let mut first = message(AgentRole::Tool, "file contents");
    first.tool_call_id = Some("call-a".into());
    first.tool_name = Some("read_file".into());
    let mut second = message(AgentRole::Tool, "/project");
    second.tool_call_id = Some("call-b".into());
    second.tool_name = Some("run_command".into());
    let input = responses::agent_input(&[
        message(AgentRole::User, "inspect"),
        assistant,
        first,
        second,
    ])
    .unwrap();
    assert_eq!(input[2]["call_id"], "call-a");
    assert_eq!(input[3]["call_id"], "call-b");
    assert_eq!(
        input[4],
        json!({"type": "function_call_output", "call_id": "call-a", "output": "file contents"})
    );
    assert!(
        !serde_json::to_string(&input)
            .unwrap()
            .contains("秘密の思考")
    );
}

#[tokio::test]
async fn failed_truncated_and_cancelled_streams_never_return_tools() {
    for value in [
        json!({"type": "response.incomplete"}),
        json!({"type": "response.failed"}),
        json!({"type": "error", "message": "provider details are not exposed"}),
    ] {
        let result = responses::consume(
            futures_util::stream::iter(vec![Ok::<_, ()>(Bytes::from(event(value)))]),
            |_| Ok(()),
        )
        .await;
        assert!(result.is_err());
    }
    let truncated = event(json!({"type": "response.output_text.delta", "delta": "partial"}));
    let error = responses::consume(
        futures_util::stream::iter(vec![Ok::<_, ()>(Bytes::from(truncated.clone()))]),
        |_| Ok(()),
    )
    .await
    .err()
    .unwrap();
    assert!(error.contains("before response.completed"));
    let disconnected = responses::consume(
        futures_util::stream::iter(vec![Ok(Bytes::from(truncated.clone())), Err(())]),
        |_| Ok(()),
    )
    .await
    .err()
    .unwrap();
    assert!(disconnected.contains("disconnected"));
    let cancelled = responses::consume(
        futures_util::stream::iter(vec![Ok::<_, ()>(Bytes::from(truncated))]),
        |_| Err(CANCELLED.into()),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(cancelled, CANCELLED);
    let malformed = completed(
        "",
        vec![
            json!({"type": "function_call", "call_id": "x", "name": "read_file", "arguments": "{\"path\":"}),
        ],
    );
    assert!(
        responses::consume(
            futures_util::stream::iter(vec![Ok::<_, ()>(Bytes::from(event(malformed)))]),
            |_| Ok(())
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn oauth_models_agent_response_and_next_tool_turn_use_taceta_wire_contract() {
    let call = json!({"type": "function_call", "id": "item", "call_id": "read-1", "name": "read_file", "arguments": "{\"path\":\"README.md\"}"});
    // Required reference version comes from Grok Build 37949780's published
    // xai-grok-version package, independently of Taceta's package version.
    let mut fixture = Fixture::start_with_required_version(vec![
        Reply::json(json!({})),
        Reply::json(json!({"data": [{"id": "account-current", "apiBackend": "responses", "contextWindow": 131072}]})),
        Reply { status: "200 OK", content_type: "text/event-stream", body: event(completed("", vec![call])) },
        Reply { status: "200 OK", content_type: "text/event-stream", body: event(completed("The file says hello.", Vec::new())) },
    ], Some("1.0.24")).await;
    let store = Arc::new(MemoryStore::default());
    saved_token(&store, false);
    let client = GrokClient::configured(
        AuthEndpoints {
            authorize: format!("{}/authorize", fixture.base),
            token: format!("{}/token", fixture.base),
        },
        format!("{}/v1", fixture.base),
        store.clone(),
    );
    assert_eq!(
        store.reads.load(Ordering::SeqCst),
        0,
        "constructor must not access credentials"
    );
    // Reproduce the omitted-header request using the same authenticated POST
    // builder as inference, with fixture credentials only.
    let mut omitted_version = client
        .request(reqwest::Method::POST, "/responses", Some("account-current"))
        .await
        .unwrap()
        .json(&json!({"model": "account-current", "input": [], "stream": true, "store": false}))
        .build()
        .unwrap();
    omitted_version
        .headers_mut()
        .remove("x-grok-client-version");
    let rejected = client.inner.http.execute(omitted_version).await.unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::UPGRADE_REQUIRED);
    let explanation = reject_status(rejected.status()).unwrap_err();
    assert!(explanation.contains("HTTP 426"));
    assert!(!explanation.contains("cannot use"));
    let models = client.list_models().await.unwrap();
    assert_eq!(models[0].name, "account-current");
    let tool = ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    };
    let (events, _receiver) = mpsc::unbounded_channel();
    let result = client
        .turn(
            AgentRequest {
                model: models[0].name.clone(),
                messages: vec![message(AgentRole::User, "Read README.md")],
                tools: vec![tool],
                thinking: ThinkingMode::Default,
                context_length: 8192,
            },
            events.clone(),
        )
        .await
        .unwrap();
    let mut assistant = message(AgentRole::Assistant, &result.content);
    assistant.tool_calls = result.tool_calls;
    let mut output = message(AgentRole::Tool, "hello");
    output.tool_call_id = Some("read-1".into());
    output.tool_name = Some("read_file".into());
    let final_turn = client
        .turn(
            AgentRequest {
                model: models[0].name.clone(),
                messages: vec![
                    message(AgentRole::User, "Read README.md"),
                    assistant,
                    output,
                ],
                tools: Vec::new(),
                thinking: ThinkingMode::Default,
                context_length: 8192,
            },
            events,
        )
        .await
        .unwrap();
    assert_eq!(final_turn.content, "The file says hello.");
    fixture.task.await.unwrap();
    let rejected_request = fixture.requests.recv().await.unwrap();
    assert_eq!(rejected_request.path, "/v1/responses");
    assert!(
        !rejected_request
            .headers
            .contains_key("x-grok-client-version")
    );
    let models_request = fixture.requests.recv().await.unwrap();
    let first = fixture.requests.recv().await.unwrap();
    let second = fixture.requests.recv().await.unwrap();
    assert_eq!(models_request.path, "/v1/models");
    assert_eq!(first.path, "/v1/responses");
    assert_eq!(first.headers["authorization"], "Bearer fixture-access-old");
    assert_eq!(first.headers["x-xai-token-auth"], "xai-grok-cli");
    assert_eq!(first.headers["x-grok-client-identifier"], "taceta");
    assert_eq!(
        first.headers["user-agent"],
        concat!("Taceta/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(first.headers["x-grok-client-version"], "1.0.24");
    assert_eq!(second.headers["x-grok-client-version"], "1.0.24");
    assert_ne!(
        first.headers["x-grok-req-id"],
        second.headers["x-grok-req-id"]
    );
    let first_body: Value = serde_json::from_slice(&first.body).unwrap();
    let second_body: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(first_body["store"], false);
    assert_eq!(first_body["tools"][0]["type"], "function");
    assert_eq!(
        first_body["max_output_tokens"],
        crate::agent::reserved_output_tokens(8192)
    );
    assert!(
        second_body.get("tools").is_none(),
        "summary turns may omit tool definitions"
    );
    assert_eq!(second_body["input"][1]["call_id"], "read-1");
    assert_eq!(second_body["input"][2]["type"], "function_call_output");
    assert_eq!(second_body["input"][2]["call_id"], "read-1");
    assert!(second_body.get("previous_response_id").is_none());
}

#[test]
fn invalid_tool_histories_are_rejected_before_transport() {
    let mut assistant = message(AgentRole::Assistant, "");
    assistant.tool_calls = vec![AgentToolCall {
        id: "a".into(),
        name: "read_file".into(),
        arguments: json!({"path": "x"}),
    }];
    assert!(responses::agent_input(&[assistant]).is_err());
    let mut output = message(AgentRole::Tool, "x");
    output.tool_call_id = Some("unknown".into());
    assert!(responses::agent_input(&[output]).is_err());
}

#[test]
fn ordinary_chat_excludes_thinking_and_interrupted_messages_and_gates_images() {
    let mut user = ChatMessage {
        id: Uuid::new_v4(),
        role: Role::User,
        content: "hello".into(),
        thinking: String::new(),
        attachments: Vec::new(),
        citations: Vec::new(),
        interrupted: false,
    };
    user.thinking = "private-thinking".into();
    user.attachments.push(Attachment {
        name: "note.txt".into(),
        payload: AttachmentPayload::Text("attachment text".into()),
    });
    let interrupted = ChatMessage {
        id: Uuid::new_v4(),
        role: Role::Assistant,
        content: "unfinished-response".into(),
        thinking: String::new(),
        attachments: Vec::new(),
        citations: Vec::new(),
        interrupted: true,
    };
    let mut request = ChatRequest {
        model: "account-current".into(),
        messages: vec![user, interrupted],
        thinking: ThinkingMode::Default,
        context_length: 8192,
        tools: None,
        web_search_provider: None,
        max_search_results: 3,
        chatgpt_web_request_limit: 1,
        fetch_search_pages: true,
        web_authorization: None,
    };
    let input = chat_input(&request, false, ApiKind::Responses).unwrap();
    let wire = serde_json::to_string(&input).unwrap();
    assert!(wire.contains("attachment text"));
    assert!(!wire.contains("private-thinking"));
    assert!(!wire.contains("unfinished-response"));
    request.messages[0].attachments.push(Attachment {
        name: "image.png".into(),
        payload: AttachmentPayload::Image {
            media_type: "image/png".into(),
            base64: "fixture-base64".into(),
        },
    });
    assert!(chat_input(&request, false, ApiKind::Responses).is_err());
    assert_eq!(
        chat_input(&request, true, ApiKind::Responses).unwrap()[0]["content"][1]["type"],
        "input_image"
    );
}

#[tokio::test]
async fn catalog_default_chat_completions_handles_tools_final_thinking_and_failure_boundaries() {
    let first_chunks = [
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": "調べます", "reasoning_content": "separate thinking"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "read-a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\""}},
            {"index": 1, "id": "read-b", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"b.txt\"}"}}
        ]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "a.txt\"}"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 45, "completion_tokens": 18}}),
    ];
    let mut first_bytes: Vec<u8> = first_chunks.into_iter().flat_map(event).collect();
    first_bytes.extend_from_slice(b"data: [DONE]\r\n\r\n");
    let final_event = json!({"choices": [{"index": 0, "delta": {"content": "Both files were read."}, "finish_reason": "stop"}]});
    let mut final_bytes = event(final_event.clone());
    final_bytes.extend_from_slice(b"data: [DONE]\n\n");
    let mut fixture = Fixture::start(vec![
        Reply::json(
            json!({"data": [{"id": "catalog-default", "contextWindow": 64000,
            "supportsReasoningEffort": true, "reasoningEfforts": ["low", "medium", "high"]}]}),
        ),
        Reply {
            status: "200 OK",
            content_type: "text/event-stream",
            body: first_bytes,
        },
        Reply {
            status: "200 OK",
            content_type: "text/event-stream",
            body: final_bytes,
        },
    ])
    .await;
    let store = Arc::new(MemoryStore::default());
    saved_token(&store, false);
    let client = GrokClient::configured(
        AuthEndpoints {
            authorize: format!("{}/authorize", fixture.base),
            token: format!("{}/token", fixture.base),
        },
        format!("{}/v1", fixture.base),
        store,
    );
    let (events, mut receiver) = mpsc::unbounded_channel();
    let tool = || ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    };
    let first = client
        .turn(
            AgentRequest {
                model: "catalog-default".into(),
                messages: vec![message(AgentRole::User, "Read both")],
                tools: vec![tool()],
                thinking: ThinkingMode::Level(ThinkingLevel::Medium),
                context_length: 8192,
            },
            events.clone(),
        )
        .await
        .unwrap();
    assert_eq!(first.content, "調べます");
    assert_eq!(first.tool_calls.len(), 2);
    assert_eq!(first.tool_calls[0].arguments, json!({"path": "a.txt"}));
    assert_eq!(first.tool_calls[1].id, "read-b");
    assert_eq!(first.prompt_tokens, Some(45));
    let mut saw_thinking = false;
    while let Ok(delta) = receiver.try_recv() {
        if let ModelDelta::Thinking(text) = delta {
            saw_thinking |= text == "separate thinking";
        }
    }
    assert!(saw_thinking);
    let mut assistant = message(AgentRole::Assistant, &first.content);
    assistant.tool_calls = first.tool_calls;
    let mut a = message(AgentRole::Tool, "A");
    a.tool_call_id = Some("read-a".into());
    a.tool_name = Some("read_file".into());
    let mut b = message(AgentRole::Tool, "B");
    b.tool_call_id = Some("read-b".into());
    b.tool_name = Some("read_file".into());
    let last = client
        .turn(
            AgentRequest {
                model: "catalog-default".into(),
                messages: vec![message(AgentRole::User, "Read both"), assistant, a, b],
                tools: vec![tool()],
                thinking: ThinkingMode::Default,
                context_length: 8192,
            },
            events,
        )
        .await
        .unwrap();
    assert_eq!(last.content, "Both files were read.");
    fixture.task.await.unwrap();
    fixture.requests.recv().await.unwrap(); // catalog request
    let first = fixture.requests.recv().await.unwrap();
    let second = fixture.requests.recv().await.unwrap();
    assert_eq!(first.path, "/v1/chat/completions");
    assert_eq!(second.path, "/v1/chat/completions");
    let first: Value = serde_json::from_slice(&first.body).unwrap();
    let second: Value = serde_json::from_slice(&second.body).unwrap();
    assert!(first.get("input").is_none());
    assert_eq!(first["tools"][0]["function"]["name"], "read_file");
    assert_eq!(first["reasoning_effort"], "medium");
    assert_eq!(first["stream_options"]["include_usage"], true);
    assert_eq!(
        first["max_tokens"],
        crate::agent::reserved_output_tokens(8192)
    );
    assert_eq!(first["store"], false);
    assert_eq!(second["messages"][1]["tool_calls"][0]["id"], "read-a");
    assert_eq!(second["messages"][2]["tool_call_id"], "read-a");
    assert_eq!(second["messages"][3]["tool_call_id"], "read-b");
    assert!(
        !serde_json::to_string(&second)
            .unwrap()
            .contains("separate thinking")
    );
    for bytes in [
        event(final_event), // successful finish_reason without [DONE] is incomplete
        b"data: [DONE]\n\n".to_vec(), // [DONE] without finish_reason is incomplete
        event(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]})),
        event(json!({"error": {"message": "provider failure"}})),
    ] {
        let chunks = bytes
            .chunks(1)
            .map(|chunk| Ok::<_, ()>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        assert!(
            completions::consume(futures_util::stream::iter(chunks), |_| Ok(()))
                .await
                .is_err()
        );
    }
}
