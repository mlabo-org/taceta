use super::*;
use crate::{
    agent::{AgentMessage, AgentRole, AgentToolCall, ToolDefinition},
    domain::{Attachment, AttachmentPayload, ChatMessage, ModelIdentity, ThinkingLevel, ThinkingMode},
};
use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::{get, post}, Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio::net::TcpListener;
use url::Url;

// Loopback fixture server copied from grok-codex-bridge src/grok.rs tests.
async fn start(router: Router) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
    (Url::parse(&format!("http://{address}/v1/")).unwrap(), task)
}

fn chat_request(model: &str, messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        model: model.into(), messages, thinking: ThinkingMode::Default, context_length: 32_768,
        tools: None, web_search_provider: None, max_search_results: 5,
        chatgpt_web_request_limit: 1, fetch_search_pages: false, web_authorization: None,
    }
}

fn admitted(id: &str) -> ModelInfo {
    ModelInfo::from_admitted(id.into(), &json!({"model": id, "apiBackend": "responses"}))
}

fn sse(events: &[Value], trailing_boundary: bool) -> Vec<Bytes> {
    let mut body = events.iter().map(|event| format!("data: {event}")).collect::<Vec<_>>().join("\n\n");
    if trailing_boundary { body.push_str("\n\n"); }
    // Deliberately split inside UTF-8/data framing, matching a transport stream.
    body.as_bytes().chunks(11).map(Bytes::copy_from_slice).collect()
}

#[tokio::test]
async fn bridge_model_switch_uses_exact_catalog_slug_and_transport_contract() {
    type Captures = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
    async fn models(headers: HeaderMap) -> impl IntoResponse {
        assert_eq!(headers["authorization"], "Bearer fixture-token");
        assert_eq!(headers["x-userid"], "fixture-user");
        assert_eq!(headers["x-grok-user-id"], "fixture-user");
        Json(json!({"data": [
            {"id":"display-45", "model":"grok-4.5", "apiBackend":"responses"},
            {"id":"display-46", "modelId":"grok-4.6", "api_backend":"responses", "baseUrl":"https://cli-chat-proxy.grok.com/v1/"}
        ]}))
    }
    async fn response(State(captures): State<Captures>, headers: HeaderMap, Json(body): Json<Value>) -> impl IntoResponse {
        let selected = body["model"].as_str().unwrap().to_owned();
        captures.lock().unwrap().push((headers, body));
        let created = json!({"type":"response.created", "response":{"id":"resp_fixture", "model":format!("{selected}-build")}});
        let completed = json!({"type":"response.completed", "response":{"id":"resp_fixture", "model":format!("{selected}-build"), "status":"completed", "output":[{"type":"message", "id":"msg_fixture", "content":[{"type":"output_text", "text":"ready"}]}]}});
        ([("content-type", "text/event-stream")], format!("data: {created}\n\ndata: {completed}"))
    }
    let captures = Captures::default();
    let router = Router::new().route("/v1/models", get(models))
        .route("/v1/responses", post(response)).with_state(captures.clone());
    let (base, task) = start(router).await;
    let transport = transport::GrokClient::for_test(base).unwrap();
    let credential = Arc::new(auth::SessionCredential::for_test("fixture-token", "fixture-user"));
    let fetched = transport.fetch_models(&credential).await.unwrap();
    assert_eq!(fetched.models, ["grok-4.5", "grok-4.6"]);
    let mut messages = vec![ChatMessage::new_system("Existing user instructions."), ChatMessage::new_user("日本語で応答して。")];
    for (id, entry) in fetched.models.into_iter().zip(fetched.entries) {
        let model = ModelInfo::from_admitted(id.clone(), &entry);
        let chat = chat_request(&id, messages.clone());
        let request = adapter::request(&model, adapter::chat_input(&chat, false).unwrap(), &[], ThinkingMode::Default).unwrap();
        let events = start_responses(&transport, credential.clone(), &request).await.unwrap();
        let mut reported = Vec::new();
        let turn = adapter::consume(events, &[], |event| {
            if let OutputDelta::ReportedModel(model) = event { reported.push(model); }
            Ok(())
        }).await.unwrap();
        assert_eq!(turn.content, "ready");
        assert_eq!(reported, [format!("{id}-build")]);
        messages.push(ChatMessage::new_assistant("ready"));
        messages.push(ChatMessage::new_user("次の質問です。"));
    }
    let captured = captures.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for ((headers, body), expected) in captured.iter().zip(["grok-4.5", "grok-4.6"]) {
        assert_eq!(body["model"], expected);
        assert_eq!(headers["x-grok-model-override"], expected);
        assert_eq!(headers["x-xai-token-auth"], "xai-grok-cli");
        assert_eq!(headers["x-authenticateresponse"], "authenticate-response");
        assert_eq!(headers["x-userid"], "fixture-user");
        assert_eq!(headers["x-grok-user-id"], "fixture-user");
        assert_eq!(headers["x-grok-client-identifier"], "grok-shell");
        assert_eq!(headers["x-grok-client-mode"], "headless");
        assert_eq!(headers["x-grok-client-version"], "1.0.5");
        assert_eq!(headers["x-grok-turn-idx"], "1");
        assert_eq!(headers["x-grok-session-id"], headers["x-grok-conv-id"]);
        assert_eq!(headers["accept"], "text/event-stream");
        assert_eq!(body["instructions"], "Existing user instructions.");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }
    assert_eq!(captured[0].0["x-grok-conv-id"], captured[1].0["x-grok-conv-id"]);
    assert_ne!(captured[0].0["x-grok-req-id"], captured[1].0["x-grok-req-id"]);
    assert_ne!(captured[0].0["x-grok-agent-id"], captured[1].0["x-grok-agent-id"]);
    assert_eq!(captured[1].1["input"][1]["content"], "ready");
    assert_eq!(captured[1].1["input"].as_array().unwrap().len(), 3);
    task.abort();
}

#[tokio::test]
async fn bridge_stream_projects_thinking_tools_terminal_output_and_raw_model() {
    let call = json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"read_file", "arguments":"{\"path\":\"src/main.rs\"}"});
    let message = json!({"type":"message", "id":"msg_1", "role":"assistant", "content":[{"type":"output_text", "text":"確認します。"}]});
    let reasoning = json!({"type":"reasoning", "id":"rs_1", "summary":[{"type":"summary_text", "text":"調べます。"}]});
    let events = vec![
        json!({"type":"response.created", "response":{"id":"resp_1", "model":"grok-4.6-build"}}),
        json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"reasoning", "id":"rs_1"}}),
        json!({"type":"response.reasoning_summary_text.delta", "item_id":"rs_1", "output_index":0, "summary_index":0, "delta":"調べます。"}),
        json!({"type":"response.reasoning_summary_text.done", "item_id":"rs_1", "output_index":0, "summary_index":0, "text":"調べます。"}),
        json!({"type":"response.output_item.done", "output_index":0, "item":reasoning.clone()}),
        json!({"type":"response.output_item.added", "output_index":1, "item":{"type":"message", "id":"msg_1", "role":"assistant", "content":[]}}),
        json!({"type":"response.output_text.delta", "item_id":"msg_1", "output_index":1, "content_index":0, "delta":"確認"}),
        json!({"type":"response.output_text.delta", "item_id":"msg_1", "output_index":1, "content_index":0, "delta":"します。"}),
        json!({"type":"response.output_text.done", "item_id":"msg_1", "output_index":1, "content_index":0, "text":"確認します。"}),
        json!({"type":"response.output_item.done", "output_index":1, "item":message.clone()}),
        json!({"type":"response.output_item.added", "output_index":2, "item":{"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"read_file", "arguments":""}}),
        json!({"type":"response.function_call_arguments.delta", "item_id":"fc_1", "output_index":2, "delta":"{\"path\":\"src/main.rs\"}"}),
        json!({"type":"response.function_call_arguments.done", "item_id":"fc_1", "output_index":2, "arguments":"{\"path\":\"src/main.rs\"}"}),
        json!({"type":"response.output_item.done", "output_index":2, "item":call.clone()}),
        json!({"type":"response.completed", "response":{"id":"resp_1", "model":"grok-4.6-build", "status":"completed", "output":[reasoning, message, call], "usage":{"input_tokens":12, "output_tokens":34}}}),
    ];
    let tools = vec![ToolDefinition { name:"read_file".into(), description:"Read file".into(), parameters:json!({"type":"object", "properties":{"path":{"type":"string"}}, "required":["path"]}) }];
    let mut text = String::new();
    let mut thinking = String::new();
    let mut models = Vec::new();
    let turn = adapter::consume(transport::fixture_events(sse(&events, false)), &tools, |event| {
        match event { OutputDelta::Content(delta) => text.push_str(&delta), OutputDelta::Thinking(delta) => thinking.push_str(&delta), OutputDelta::ReportedModel(model) => models.push(model) }
        Ok(())
    }).await.unwrap();
    assert_eq!(turn.content, "確認します。");
    assert_eq!(text, turn.content);
    assert_eq!(thinking, "調べます。");
    assert_eq!(models, ["grok-4.6-build"]);
    assert_eq!(turn.tool_calls, [AgentToolCall { id:"call_1".into(), name:"read_file".into(), arguments:json!({"path":"src/main.rs"}) }]);
    assert_eq!(turn.prompt_tokens, Some(12));
    assert_eq!(turn.completion_tokens, Some(34));
}

#[test]
fn bridge_request_projection_preserves_full_history_and_excludes_thinking() {
    let model = ModelInfo::from_admitted("grok-4.6".into(), &json!({"supportsVision":true, "supportsReasoningEffort":true, "reasoningEfforts":["low", "medium", "high"]}));
    let mut user = ChatMessage::new_user("Original prompt.");
    user.attachments.push(Attachment { name:"note.txt".into(), payload:AttachmentPayload::Text("Attached text.".into()) });
    user.attachments.push(Attachment { name:"pixel.png".into(), payload:AttachmentPayload::Image { media_type:"image/png".into(), base64:"aGVsbG8=".into() } });
    let mut assistant = ChatMessage::new_assistant("Prior final answer.");
    assistant.thinking = "PRIVATE_THINKING_MARKER".into();
    assistant.model_identity = Some(ModelIdentity { requested_model:"DO_NOT_REPLAY_MODEL".into(), reported_model:Some("DO_NOT_REPLAY_RESPONSE".into()) });
    let mut interrupted = ChatMessage::new_assistant("INTERRUPTED_MARKER");
    interrupted.interrupted = true;
    let chat = chat_request("grok-4.6", vec![ChatMessage::new_system("Existing instructions: keep  two spaces."), user, assistant, interrupted, ChatMessage::new_user("Next prompt.")]);
    let body = adapter::request(&model, adapter::chat_input(&chat, true).unwrap(), &[], ThinkingMode::Level(ThinkingLevel::High)).unwrap().to_xai_value();
    assert_eq!(body["instructions"], "Existing instructions: keep  two spaces.");
    assert_eq!(body["input"][0]["content"][0]["text"], "Original prompt.\n\n[Attachment: note.txt]\nAttached text.");
    assert_eq!(body["input"][0]["content"][1]["image_url"], "data:image/png;base64,aGVsbG8=");
    assert_eq!(body["input"][1]["content"], "Prior final answer.");
    assert_eq!(body["input"][2]["content"][0]["text"], "Next prompt.");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
    let encoded = body.to_string();
    for marker in ["PRIVATE_THINKING_MARKER", "DO_NOT_REPLAY_MODEL", "DO_NOT_REPLAY_RESPONSE", "INTERRUPTED_MARKER"] { assert!(!encoded.contains(marker)); }

    let call = AgentToolCall { id:"call_1".into(), name:"read_file".into(), arguments:json!({"line":12.0}) };
    let messages = vec![
        AgentMessage::text(AgentRole::System, "Exact agent instructions."),
        AgentMessage::text(AgentRole::User, "Read source."),
        AgentMessage { role:AgentRole::Assistant, content:String::new(), tool_calls:vec![call], tool_call_id:None, tool_name:None },
        AgentMessage { role:AgentRole::Tool, content:"file contents".into(), tool_calls:vec![], tool_call_id:Some("call_1".into()), tool_name:Some("read_file".into()) },
        AgentMessage::text(AgentRole::User, "Continue."),
    ];
    let tool = ToolDefinition { name:"read_file".into(), description:"Read source".into(), parameters:json!({"type":"object", "properties":{"line":{"type":"integer"}}}) };
    let body = adapter::request(&admitted("grok-4.5"), adapter::agent_input(&messages).unwrap(), &[tool], ThinkingMode::Default).unwrap().to_xai_value();
    assert_eq!(body["model"], "grok-4.5");
    assert_eq!(body["instructions"], "Exact agent instructions.");
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][1]["call_id"], "call_1");
    assert_eq!(body["input"][1]["arguments"], "{\"line\":12}");
    assert_eq!(body["input"][2], json!({"type":"function_call_output", "call_id":"call_1", "output":"file contents"}));
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tool_choice"], "auto");
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn bridge_stream_preserves_reference_eof_and_failure_behavior() {
    let events = vec![
        json!({"type":"response.created", "response":{"id":"resp_eof"}}),
        json!({"type":"response.output_text.delta", "item_id":"msg_eof", "output_index":0, "delta":"streamed answer"}),
    ];
    let turn = adapter::consume(transport::fixture_events(sse(&events, false)), &[], |_| Ok(())).await.unwrap();
    assert_eq!(turn.content, "streamed answer");
    for terminal in ["response.failed", "response.incomplete"] {
        let mut failed = events.clone();
        failed.push(json!({"type":terminal, "response":{"id":"resp_eof"}}));
        assert!(adapter::consume(transport::fixture_events(sse(&failed, false)), &[], |_| Ok(())).await.is_err());
    }
    assert!(adapter::consume(transport::fixture_events(vec![Bytes::from_static(b"data: not-json\n\n")]), &[], |_| Ok(())).await.is_err());
}

#[tokio::test]
async fn bridge_stream_failure_preserves_cause_after_thinking() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    type Fixture = (
        Arc<Mutex<Option<oneshot::Receiver<()>>>>,
        Arc<AtomicUsize>,
    );

    async fn interrupted_response(State((gate, requests)): State<Fixture>) -> impl IntoResponse {
        requests.fetch_add(1, Ordering::SeqCst);
        let receiver = gate.lock().unwrap().take().unwrap();
        let created = json!({
            "type": "response.created",
            "response": {"id": "resp_fixture", "model": "grok-4.6-build"}
        });
        let thinking = json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_fixture", "output_index": 0, "summary_index": 0,
            "delta": "private fixture thinking"
        });
        let prefix = Bytes::from(format!("data: {created}\n\ndata: {thinking}\n\n"));
        let body = stream::once(async move { Ok::<_, std::io::Error>(prefix) })
            .chain(stream::once(async move {
                // Do not close until Taceta has actually consumed Thinking.
                // Dropping this HTTP body without its final chunk reproduces
                // a receive failure rather than a clean SSE EOF.
                let _ = receiver.await;
                Err::<Bytes, _>(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "private fixture transport detail",
                ))
            }));
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(body),
        )
    }

    let (release, receiver) = oneshot::channel();
    let requests = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/v1/responses", post(interrupted_response))
        .with_state((Arc::new(Mutex::new(Some(receiver))), requests.clone()));
    let (base, server) = start(router).await;
    let client = transport::GrokClient::for_test(base.clone()).unwrap();
    let credential = Arc::new(auth::SessionCredential::for_test("fixture-token", "fixture-user"));
    let request = adapter::request(
        &admitted("grok-4.6"),
        adapter::chat_input(
            &chat_request("grok-4.6", vec![ChatMessage::new_user("fixture request")]),
            false,
        ).unwrap(),
        &[],
        ThinkingMode::Default,
    ).unwrap();
    let upstream = start_responses(&client, credential, &request).await.unwrap();
    let captured_failure = Arc::new(Mutex::new(None));
    let failure_for_stream = captured_failure.clone();
    let upstream = upstream.inspect(move |result| {
        if let Err(GrokError::Sse { failure, state }) = result {
            *failure_for_stream.lock().unwrap() = Some((failure.clone(), *state));
        }
    });
    let mut release = Some(release);
    let mut received_thinking = false;
    let error = adapter::consume(upstream, &[], |delta| {
        if let OutputDelta::Thinking(_) = delta {
            received_thinking = true;
            release.take().unwrap().send(()).unwrap();
        }
        Ok(())
    }).await.unwrap_err();

    assert!(received_thinking);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        *captured_failure.lock().unwrap(),
        Some((
            // reqwest 0.13.4 bytes_stream maps receive-body errors through
            // error::decode; this is still a transport failure, not SSE JSON.
            transport::SseFailure::Transport { timeout: false, body: false, decode: true },
            protocol::TextStreamState::Streaming,
        )),
    );
    assert_eq!(error, "xAI Responses stream failed (source=transport; timeout=false; body=false; decode=true; state=Streaming)");
    for private in [
        "private fixture thinking", "private fixture transport detail",
        "fixture-token", "fixture-user", base.as_str(),
    ] {
        assert!(!error.contains(private));
    }
    server.abort();
}
