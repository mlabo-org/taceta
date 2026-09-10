use crate::agent::{AgentMessage, AgentRole, AgentToolCall, AgentTurn, ToolDefinition};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(super) const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_TOOL_CALLS: usize = 128;

pub(super) enum OutputDelta {
    Content(String),
    Thinking(String),
}

pub(super) fn agent_input(messages: &[AgentMessage]) -> Result<Vec<Value>, String> {
    super::validate_agent_messages(messages)?;
    let mut input = Vec::new();
    for message in messages {
        if matches!(message.role, AgentRole::Tool) {
            input.push(json!({"type": "function_call_output", "call_id": message.tool_call_id, "output": message.content}));
            continue;
        }
        let role = match message.role {
            AgentRole::System => "system",
            AgentRole::User => "user",
            AgentRole::Assistant => "assistant",
            AgentRole::Tool => unreachable!(),
        };
        if !message.content.is_empty() || message.tool_calls.is_empty() {
            input.push(json!({"role": role, "content": message.content}));
        }
        for call in &message.tool_calls {
            input.push(json!({"type": "function_call", "call_id": call.id, "name": call.name,
                "arguments": serde_json::to_string(&call.arguments).map_err(|_| "Unable to encode Grok function arguments.")?}));
        }
    }
    Ok(input)
}

pub(super) fn tools(definitions: &[ToolDefinition]) -> Result<Vec<Value>, String> {
    super::validate_tools(definitions)?;
    definitions.iter().map(|tool| {
        Ok(json!({"type": "function", "name": tool.name, "description": tool.description, "parameters": tool.parameters}))
    }).collect()
}

pub(super) async fn consume<S, E>(
    mut stream: S,
    mut emit: impl FnMut(OutputDelta) -> Result<(), String>,
) -> Result<(AgentTurn, Option<String>), String>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let mut decoder = SseDecoder::default();
    let mut state = ResponseState::default();
    let mut received = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| "The Grok response stream disconnected before completion.")?;
        received = received.saturating_add(chunk.len());
        if received > MAX_STREAM_BYTES {
            return Err("Grok response exceeded Taceta's stream size limit.".into());
        }
        for (kind, payload) in decoder.push(&chunk)? {
            if payload.trim() == "[DONE]" {
                return Err("Grok ended the stream without response.completed.".into());
            }
            let event: Value = serde_json::from_str(&payload)
                .map_err(|_| "Grok returned an invalid streaming event.")?;
            if let Some(result) = state.event(event, kind.as_deref(), &mut emit)? {
                return Ok(result);
            }
        }
    }
    Err("The Grok response stream ended before response.completed.".into())
}

#[derive(Default)]
pub(super) struct SseDecoder {
    buffer: Vec<u8>,
    data: String,
    event: Option<String>,
}

impl SseDecoder {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<(Option<String>, String)>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut consumed = 0;
        while let Some(end) = self.buffer[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + end;
            let bytes = self.buffer[consumed..end]
                .strip_suffix(b"\r")
                .unwrap_or(&self.buffer[consumed..end]);
            let line = std::str::from_utf8(bytes)
                .map_err(|_| "Grok returned invalid UTF-8 in its stream.")?;
            consumed = end + 1;
            if line.is_empty() {
                if !self.data.is_empty() {
                    self.data.pop(); // final separator added after the last data line
                    events.push((self.event.take(), std::mem::take(&mut self.data)));
                } else {
                    self.event = None;
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                self.data.push('\n');
            } else if let Some(event) = line.strip_prefix("event:") {
                self.event = Some(event.strip_prefix(' ').unwrap_or(event).to_owned());
            }
            if self.data.len() > MAX_EVENT_BYTES {
                return Err("A Grok stream event exceeded Taceta's size limit.".into());
            }
        }
        self.buffer.drain(..consumed);
        if self.buffer.len() + self.data.len() > MAX_EVENT_BYTES {
            return Err("A Grok stream event exceeded Taceta's size limit.".into());
        }
        Ok(events)
    }
}

#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ResponseState {
    content: String,
    calls: HashMap<String, PendingCall>,
    reported_model: Option<String>,
}

impl ResponseState {
    fn event(
        &mut self,
        event: Value,
        hint: Option<&str>,
        emit: &mut impl FnMut(OutputDelta) -> Result<(), String>,
    ) -> Result<Option<(AgentTurn, Option<String>)>, String> {
        super::record_reported_model(
            &mut self.reported_model,
            event
                .get("response")
                .and_then(|response| response.get("model")),
        )?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .or(hint)
            .ok_or("A Grok stream event has no type.")?;
        match kind {
            "response.output_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or("Grok text delta is invalid.")?;
                self.content.push_str(delta);
                emit(OutputDelta::Content(delta.into()))?;
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or("Grok Thinking delta is invalid.")?;
                emit(OutputDelta::Thinking(delta.into()))?;
            }
            "response.output_item.added" | "response.output_item.done" => {
                let item = event.get("item").ok_or("Grok output item is missing.")?;
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let key = item
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            event
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .map(|index| format!("index:{index}"))
                        })
                        .ok_or("Grok function call has no output identity.")?;
                    let call = self.calls.entry(key).or_default();
                    update_identity(call, item)?;
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        if !arguments.is_empty() {
                            complete_arguments(&mut call.arguments, arguments)?;
                        }
                    }
                    if self.calls.len() > MAX_TOOL_CALLS {
                        return Err("Grok returned too many function calls.".into());
                    }
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let key = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .map(|index| format!("index:{index}"))
                    })
                    .ok_or("Grok function arguments have no output identity.")?;
                let call = self.calls.entry(key).or_default();
                if kind.ends_with(".delta") {
                    call.arguments.push_str(
                        event
                            .get("delta")
                            .and_then(Value::as_str)
                            .ok_or("Grok function argument delta is invalid.")?,
                    );
                } else {
                    complete_arguments(
                        &mut call.arguments,
                        event
                            .get("arguments")
                            .and_then(Value::as_str)
                            .ok_or("Grok completed function arguments are invalid.")?,
                    )?;
                }
                if call.arguments.len() > MAX_EVENT_BYTES || self.calls.len() > MAX_TOOL_CALLS {
                    return Err("Grok function arguments exceeded Taceta's size limit.".into());
                }
            }
            "response.completed" => {
                return self
                    .complete(
                        event
                            .get("response")
                            .ok_or("Grok completed response is missing.")?,
                        emit,
                    )
                    .map(Some);
            }
            "response.incomplete" => {
                return Err(
                    "Grok stopped with an incomplete response. No pending tools were executed."
                        .into(),
                );
            }
            "response.failed" | "response.error" | "error" => {
                return Err(
                    "Grok reported a response failure. No pending tools were executed.".into(),
                );
            }
            // Lifecycle, content-part and usage announcements are not output.
            _ => {}
        }
        Ok(None)
    }

    fn complete(
        &self,
        response: &Value,
        emit: &mut impl FnMut(OutputDelta) -> Result<(), String>,
    ) -> Result<(AgentTurn, Option<String>), String> {
        if response.get("status").and_then(Value::as_str) != Some("completed")
            || response.get("error").is_some_and(|error| !error.is_null())
        {
            return Err("Grok did not report successful response completion.".into());
        }
        let output = response
            .get("output")
            .and_then(Value::as_array)
            .ok_or("Grok completed response has no output array.")?;
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut ids = HashSet::new();
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let parts = item
                        .get("content")
                        .and_then(Value::as_array)
                        .ok_or("Grok output message is invalid.")?;
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => content.push_str(
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .ok_or("Grok completed text is invalid.")?,
                            ),
                            Some("refusal") => content.push_str(
                                part.get("refusal")
                                    .and_then(Value::as_str)
                                    .ok_or("Grok refusal text is invalid.")?,
                            ),
                            _ => {
                                return Err(
                                    "Grok returned an unsupported message content type.".into()
                                );
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or("Grok function call has no call ID.")?;
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or("Grok function call has no name.")?;
                    if !ids.insert(id.to_owned()) || tool_calls.len() >= MAX_TOOL_CALLS {
                        return Err("Grok returned duplicate or excessive function calls.".into());
                    }
                    let raw = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .ok_or("Grok function arguments are missing.")?;
                    let arguments: Value = serde_json::from_str(raw)
                        .map_err(|_| "Grok function arguments are not complete JSON.")?;
                    if !arguments.is_object() {
                        return Err("Grok function arguments must be a JSON object.".into());
                    }
                    if let Some(pending) = self.calls.values().find(|pending| pending.call_id == id)
                    {
                        if (!pending.name.is_empty() && pending.name != name)
                            || !raw.starts_with(&pending.arguments)
                        {
                            return Err("Grok's completed function call disagrees with its streamed arguments.".into());
                        }
                    }
                    tool_calls.push(AgentToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments,
                    });
                }
                Some("reasoning") => {}
                _ => {
                    return Err(
                        "Grok returned an unsupported output item. No pending tools were executed."
                            .into(),
                    );
                }
            }
        }
        for pending in self.calls.values() {
            if pending.call_id.is_empty() || !ids.contains(&pending.call_id) {
                return Err(
                    "Grok completed without a function call announced in its stream.".into(),
                );
            }
        }
        if !content.starts_with(&self.content) {
            return Err("Grok's completed text disagrees with its streamed text.".into());
        }
        let suffix = &content[self.content.len()..];
        if !suffix.is_empty() {
            emit(OutputDelta::Content(suffix.into()))?;
        }
        let usage = response.get("usage");
        Ok((
            AgentTurn {
                content,
                tool_calls,
                prompt_tokens: usage
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(Value::as_u64),
                completion_tokens: usage
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(Value::as_u64),
            },
            self.reported_model.clone(),
        ))
    }
}

fn update_identity(call: &mut PendingCall, item: &Value) -> Result<(), String> {
    for (target, key) in [(&mut call.call_id, "call_id"), (&mut call.name, "name")] {
        if let Some(value) = item
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            if !target.is_empty() && target != value {
                return Err("Grok changed a streamed function call's identity.".into());
            }
            *target = value.into();
        }
    }
    Ok(())
}

fn complete_arguments(buffer: &mut String, completed: &str) -> Result<(), String> {
    if !completed.starts_with(buffer.as_str()) {
        return Err("Grok's completed arguments disagree with its stream.".into());
    }
    *buffer = completed.into();
    Ok(())
}
