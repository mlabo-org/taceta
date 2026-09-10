//! The catalog-selected Chat Completions protocol, including the public Grok
//! Build default. This is never attempted after a failed Responses request.
use crate::agent::{AgentMessage, AgentRole, AgentToolCall, AgentTurn, ToolDefinition};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

use super::responses::{
    MAX_EVENT_BYTES, MAX_STREAM_BYTES, MAX_TOOL_CALLS, OutputDelta, SseDecoder,
};

pub(super) fn agent_input(messages: &[AgentMessage]) -> Result<Vec<Value>, String> {
    super::validate_agent_messages(messages)?;
    messages.iter().map(|message| {
        let role = match message.role { AgentRole::System => "system", AgentRole::User => "user", AgentRole::Assistant => "assistant", AgentRole::Tool => "tool" };
        let mut value = json!({"role": role, "content": message.content});
        if let Some(id) = &message.tool_call_id { value["tool_call_id"] = json!(id); }
        if !message.tool_calls.is_empty() {
            value["tool_calls"] = Value::Array(message.tool_calls.iter().map(|call| {
                Ok(json!({"id": call.id, "type": "function", "function": {"name": call.name,
                    "arguments": serde_json::to_string(&call.arguments).map_err(|_| "Unable to encode Grok function arguments.")?}}))
            }).collect::<Result<Vec<_>, String>>()?);
        }
        Ok(value)
    }).collect()
}

pub(super) fn tools(definitions: &[ToolDefinition]) -> Result<Vec<Value>, String> {
    super::validate_tools(definitions)?;
    Ok(definitions
        .iter()
        .map(|tool| {
            json!({"type": "function", "function": {
                "name": tool.name, "description": tool.description, "parameters": tool.parameters,
            }})
        })
        .collect())
}

pub(super) async fn consume<S, E>(
    mut stream: S,
    mut emit: impl FnMut(OutputDelta) -> Result<(), String>,
) -> Result<AgentTurn, String>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let mut decoder = SseDecoder::default();
    let mut content = String::new();
    let mut calls: BTreeMap<u64, PendingCall> = BTreeMap::new();
    let mut finished = false;
    let mut received = 0usize;
    let mut prompt_tokens = None;
    let mut completion_tokens = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|_| "The Grok Chat Completions stream disconnected before completion.")?;
        received = received.saturating_add(chunk.len());
        if received > MAX_STREAM_BYTES {
            return Err("Grok response exceeded Taceta's stream size limit.".into());
        }
        for (_, payload) in decoder.push(&chunk)? {
            if payload.trim() == "[DONE]" {
                if !finished {
                    return Err("Grok ended the stream without a successful finish_reason.".into());
                }
                let mut ids = HashSet::new();
                let tool_calls = calls.into_values().map(|call| {
                    if call.id.is_empty() || call.name.is_empty() || !ids.insert(call.id.clone()) {
                        return Err("Grok returned a function call with a missing or duplicate identity.".into());
                    }
                    let arguments: Value = serde_json::from_str(&call.arguments).map_err(|_| "Grok function arguments are not complete JSON.")?;
                    if !arguments.is_object() { return Err("Grok function arguments must be a JSON object.".into()); }
                    Ok(AgentToolCall { id: call.id, name: call.name, arguments })
                }).collect::<Result<Vec<_>, String>>()?;
                return Ok(AgentTurn {
                    content,
                    tool_calls,
                    prompt_tokens,
                    completion_tokens,
                });
            }
            let event: Value = serde_json::from_str(&payload)
                .map_err(|_| "Grok returned an invalid Chat Completions event.")?;
            if event.get("error").is_some_and(|error| !error.is_null()) {
                return Err(
                    "Grok reported a Chat Completions failure. No pending tools were executed."
                        .into(),
                );
            }
            if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
                prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
                completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
            }
            let choices = event
                .get("choices")
                .and_then(Value::as_array)
                .ok_or("Grok Chat Completions event has no choices array.")?;
            if choices.len() > 1 {
                return Err("Grok returned multiple choices for a single response request.".into());
            }
            for choice in choices {
                if choice.get("index").and_then(Value::as_u64) != Some(0) || finished {
                    return Err(
                        "Grok returned an unexpected or already completed response choice.".into(),
                    );
                }
                let delta = choice
                    .get("delta")
                    .filter(|delta| delta.is_object())
                    .ok_or("Grok Chat Completions delta is invalid.")?;
                if let Some(text) = delta.get("content").filter(|text| !text.is_null()) {
                    let text = text.as_str().ok_or("Grok text delta is invalid.")?;
                    content.push_str(text);
                    emit(OutputDelta::Content(text.into()))?;
                }
                if let Some(text) = delta
                    .get("reasoning_content")
                    .filter(|text| !text.is_null())
                {
                    emit(OutputDelta::Thinking(
                        text.as_str()
                            .ok_or("Grok Thinking delta is invalid.")?
                            .into(),
                    ))?;
                }
                if let Some(functions) = delta.get("tool_calls").filter(|calls| !calls.is_null()) {
                    for function in functions
                        .as_array()
                        .ok_or("Grok function call delta is invalid.")?
                    {
                        let index = function
                            .get("index")
                            .and_then(Value::as_u64)
                            .ok_or("Grok function call has no stream index.")?;
                        if index >= MAX_TOOL_CALLS as u64 {
                            return Err("Grok returned too many function calls.".into());
                        }
                        let call = calls.entry(index).or_default();
                        if function
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| kind != "function")
                        {
                            return Err("Grok returned an unsupported tool call type.".into());
                        }
                        if let Some(id) = function.get("id").and_then(Value::as_str) {
                            identity(&mut call.id, id)?;
                        }
                        if let Some(function) = function.get("function") {
                            if let Some(name) = function.get("name").and_then(Value::as_str) {
                                identity(&mut call.name, name)?;
                            }
                            if let Some(arguments) = function
                                .get("arguments")
                                .filter(|arguments| !arguments.is_null())
                            {
                                call.arguments.push_str(
                                    arguments
                                        .as_str()
                                        .ok_or("Grok function arguments are invalid.")?,
                                );
                            }
                        }
                        if call.arguments.len() > MAX_EVENT_BYTES {
                            return Err(
                                "Grok function arguments exceeded Taceta's size limit.".into()
                            );
                        }
                    }
                }
                if let Some(reason) = choice
                    .get("finish_reason")
                    .filter(|reason| !reason.is_null())
                {
                    match reason.as_str() {
                        Some("stop") if calls.is_empty() => finished = true,
                        Some("tool_calls") if !calls.is_empty() => finished = true,
                        Some("length") => return Err("Grok stopped with an incomplete response. No pending tools were executed.".into()),
                        _ => return Err("Grok did not report successful Chat Completions termination. No pending tools were executed.".into()),
                    }
                }
            }
        }
    }
    Err("The Grok Chat Completions stream ended before [DONE].".into())
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

fn identity(target: &mut String, value: &str) -> Result<(), String> {
    if !target.is_empty() && target != value {
        return Err("Grok changed a streamed function call's identity.".into());
    }
    *target = value.into();
    Ok(())
}
