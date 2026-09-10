//! Taceta domain input/output conversion. The copied bridge protocol owns the
//! complete Grok request projection, routing, and streaming interpretation.
use super::{protocol::{NormalizedResponsesRequest, TextStreamEventKind, ValidatedTextStreamEvent}, transport::GrokError};
use crate::{
    agent::{AgentMessage, AgentRole, AgentToolCall, AgentTurn, ToolDefinition},
    domain::{AttachmentPayload, ChatRequest, ModelDescriptor, Role, ThinkingCapability, ThinkingLevel, ThinkingMode},
};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone)]
pub(super) struct ModelInfo {
    pub(super) descriptor: ModelDescriptor,
    efforts: HashSet<String>,
}

impl ModelInfo {
    /// Only called after the copied transport has admitted the source entry and
    /// resolved its exact model slug. These fields affect Taceta controls only.
    pub(super) fn from_admitted(name: String, entry: &Value) -> Self {
        let field = |names: &[&str]| {
            names.iter().find_map(|name| entry.get(*name))
                .or_else(|| names.iter().find_map(|name| entry.get("_meta").and_then(|meta| meta.get(*name))))
        };
        let efforts = if field(&["supportsReasoningEffort", "supports_reasoning_effort"]).and_then(Value::as_bool) == Some(true) {
            field(&["reasoningEfforts", "reasoning_efforts"]).and_then(Value::as_array)
                .into_iter().flatten().filter_map(|value| value.as_str().or_else(|| value.get("value").and_then(Value::as_str)))
                .filter(|effort| matches!(*effort, "low" | "medium" | "high"))
                .map(str::to_owned).collect::<HashSet<_>>()
        } else { HashSet::new() };
        let thinking = if ["low", "medium", "high"].iter().all(|effort| efforts.contains(*effort)) {
            ThinkingCapability::Levels
        } else { ThinkingCapability::Unverified };
        let vision = field(&["supportsVision", "supports_vision"]).and_then(Value::as_bool) == Some(true)
            || field(&["inputModalities", "input_modalities"]).and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("image")));
        let tools = field(&["supportsTools", "supports_tools", "supportsFunctionCalling", "supports_function_calling"])
            .and_then(Value::as_bool).unwrap_or(true);
        let context_length = field(&["contextWindow", "context_window", "totalContextTokens"])
            .and_then(Value::as_u64).and_then(|size| u32::try_from(size).ok()).filter(|size| *size > 0);
        Self { descriptor: ModelDescriptor { name, size: 0, thinking, vision, tools, context_length }, efforts }
    }

    fn reasoning(&self, mode: ThinkingMode) -> Result<Option<Value>, String> {
        let effort = match mode {
            ThinkingMode::Default => return Ok(None),
            ThinkingMode::Level(ThinkingLevel::Low) => "low",
            ThinkingMode::Level(ThinkingLevel::Medium) => "medium",
            ThinkingMode::Level(ThinkingLevel::High) => "high",
            ThinkingMode::Off | ThinkingMode::On => return Err("This Grok model does not advertise a boolean Thinking control. Use the model default.".into()),
        };
        if !self.efforts.contains(effort) {
            return Err("The selected Thinking level is not confirmed by this Grok model's metadata.".into());
        }
        Ok(Some(json!({"effort": effort})))
    }
}

pub(super) fn request(model: &ModelInfo, mut input: Vec<Value>, tools: &[ToolDefinition], thinking: ThinkingMode) -> Result<NormalizedResponsesRequest, String> {
    let mut body = json!({
        "model": model.descriptor.name,
        "tools": tools.iter().map(|tool| json!({
            "type": "function", "name": tool.name, "description": tool.description,
            "parameters": tool.parameters,
        })).collect::<Vec<_>>(),
        "tool_choice": "auto", "parallel_tool_calls": true,
        "store": false, "stream": true, "include": [],
    });
    // Preserve an existing leading system message verbatim in the same field
    // the bridge receives from its caller. No model-identity text is added.
    if input.first().is_some_and(|item| item["role"] == "developer") {
        let first = input.remove(0);
        body["instructions"] = first["content"][0]["text"].clone();
    }
    body["input"] = Value::Array(input);
    if let Some(reasoning) = model.reasoning(thinking)? { body["reasoning"] = reasoning; }
    NormalizedResponsesRequest::parse(body).map_err(|e| e.to_string())
}

fn message(role: &str, text: String) -> Value {
    let kind = if role == "assistant" { "output_text" } else { "input_text" };
    json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
}

pub(super) fn chat_input(request: &ChatRequest, vision: bool) -> Result<Vec<Value>, String> {
    let mut input = Vec::new();
    for item in request.messages.iter().filter(|item| !item.interrupted) {
        let role = match item.role { Role::System => "developer", Role::User => "user", Role::Assistant => "assistant" };
        let mut text = item.content.clone();
        let mut images = Vec::new();
        for attachment in &item.attachments {
            match &attachment.payload {
                AttachmentPayload::Text(body) => text.push_str(&format!("\n\n[Attachment: {}]\n{body}", attachment.name)),
                AttachmentPayload::Image { media_type, base64 } => {
                    if !vision || item.role != Role::User { return Err("Image input is not confirmed for the selected Grok model and message role.".into()); }
                    images.push(json!({"type": "input_image", "image_url": format!("data:{media_type};base64,{base64}")}));
                }
            }
        }
        let mut value = message(role, text);
        value["content"].as_array_mut().expect("message creates an array").extend(images);
        // Thinking and recorded model identity are display data, never input.
        input.push(value);
    }
    Ok(input)
}

pub(super) fn agent_input(messages: &[AgentMessage]) -> Result<Vec<Value>, String> {
    let mut input = Vec::new();
    for item in messages {
        if item.role == AgentRole::Tool {
            input.push(json!({"type": "function_call_output", "call_id": item.tool_call_id, "output": item.content}));
            continue;
        }
        if item.role != AgentRole::Assistant && !item.tool_calls.is_empty() {
            return Err("Only an assistant message can contain Taceta function calls.".into());
        }
        let role = match item.role { AgentRole::System => "developer", AgentRole::User => "user", AgentRole::Assistant => "assistant", AgentRole::Tool => unreachable!() };
        if !item.content.is_empty() || item.tool_calls.is_empty() { input.push(message(role, item.content.clone())); }
        for call in &item.tool_calls {
            input.push(json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": call.arguments.to_string()}));
        }
    }
    Ok(input)
}

pub(super) enum OutputDelta {
    Content(String),
    Thinking(String),
    ReportedModel(String),
}

#[derive(Default)]
struct FunctionCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Output {
    text: BTreeMap<(u64, u64), String>,
    thinking: BTreeMap<(u64, bool, u64), String>,
    calls: BTreeMap<u64, FunctionCall>,
    item_indices: HashMap<String, u64>,
    model: Option<String>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

impl Output {
    fn index(&self, event: &Value) -> u64 {
        event.get("output_index").and_then(Value::as_u64).or_else(|| {
            event.get("item_id").or_else(|| event.get("item").and_then(|item| item.get("id")))
                .and_then(Value::as_str).and_then(|id| self.item_indices.get(id).copied())
        }).unwrap_or(0)
    }

    fn snapshot_text(&mut self, index: u64, part: u64, text: &str, emit: &mut impl FnMut(OutputDelta) -> Result<(), String>) -> Result<(), String> {
        let previous = self.text.entry((index, part)).or_default();
        if let Some(suffix) = text.strip_prefix(previous.as_str()).filter(|suffix| !suffix.is_empty()) {
            emit(OutputDelta::Content(suffix.to_owned()))?;
        }
        *previous = text.to_owned();
        Ok(())
    }

    fn snapshot_thinking(&mut self, index: u64, summary: bool, part: u64, text: &str, emit: &mut impl FnMut(OutputDelta) -> Result<(), String>) -> Result<(), String> {
        let previous = self.thinking.entry((index, summary, part)).or_default();
        if let Some(suffix) = text.strip_prefix(previous.as_str()).filter(|suffix| !suffix.is_empty()) {
            emit(OutputDelta::Thinking(suffix.to_owned()))?;
        }
        *previous = text.to_owned();
        Ok(())
    }

    fn item(&mut self, index: u64, item: &Value, emit: &mut impl FnMut(OutputDelta) -> Result<(), String>) -> Result<(), String> {
        if let Some(id) = item.get("id").and_then(Value::as_str) { self.item_indices.insert(id.to_owned(), index); }
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for (part, value) in parts.iter().enumerate() {
                        if let Some(text) = value.get("text").or_else(|| value.get("refusal")).and_then(Value::as_str) {
                            self.snapshot_text(index, part as u64, text, emit)?;
                        }
                    }
                }
            }
            Some("function_call") => {
                let call = self.calls.entry(index).or_default();
                if let Some(id) = item.get("call_id").and_then(Value::as_str) { call.id = id.to_owned(); }
                if let Some(name) = item.get("name").and_then(Value::as_str) { call.name = name.to_owned(); }
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) { call.arguments = arguments.to_owned(); }
            }
            Some("reasoning") => {
                for (field, summary) in [("summary", true), ("content", false)] {
                    if let Some(parts) = item.get(field).and_then(Value::as_array) {
                        for (part, value) in parts.iter().enumerate() {
                            if let Some(text) = value.get("text").and_then(Value::as_str) {
                                self.snapshot_thinking(index, summary, part as u64, text, emit)?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn accept(&mut self, event: &ValidatedTextStreamEvent, emit: &mut impl FnMut(OutputDelta) -> Result<(), String>) -> Result<bool, String> {
        let value = event.original();
        let index = self.index(value);
        if let Some(model) = value.get("response").and_then(|response| response.get("model")).and_then(Value::as_str) {
            if self.model.as_deref() != Some(model) {
                self.model = Some(model.to_owned());
                emit(OutputDelta::ReportedModel(model.to_owned()))?;
            }
        }
        // Function identities and final output live in the original items;
        // the copied bridge intentionally passes provider-specific item kinds.
        if let Some(item) = value.get("item") { self.item(index, item, emit)?; }
        match event.kind() {
            TextStreamEventKind::OutputTextDelta { content_index, delta, .. } => {
                self.text.entry((index, *content_index)).or_default().push_str(delta);
                emit(OutputDelta::Content(delta.clone()))?;
            }
            TextStreamEventKind::OutputTextDone { content_index, .. }
            | TextStreamEventKind::ContentPartDone { content_index, .. } => {
                if let Some(text) = value.get("text").or_else(|| value.get("part").and_then(|part| part.get("text"))).and_then(Value::as_str) {
                    self.snapshot_text(index, *content_index, text, emit)?;
                }
            }
            TextStreamEventKind::ReasoningTextDelta { content_index, delta, .. } => {
                self.thinking.entry((index, false, *content_index)).or_default().push_str(delta);
                emit(OutputDelta::Thinking(delta.clone()))?;
            }
            TextStreamEventKind::ReasoningSummaryTextDelta { summary_index, delta, .. } => {
                self.thinking.entry((index, true, *summary_index)).or_default().push_str(delta);
                emit(OutputDelta::Thinking(delta.clone()))?;
            }
            TextStreamEventKind::ReasoningTextDone { content_index, .. } => {
                if let Some(text) = value.get("text").and_then(Value::as_str) { self.snapshot_thinking(index, false, *content_index, text, emit)?; }
            }
            TextStreamEventKind::ReasoningSummaryTextDone { summary_index, .. }
            | TextStreamEventKind::ReasoningSummaryPartDone { summary_index, .. } => {
                if let Some(text) = value.get("text").or_else(|| value.get("part").and_then(|part| part.get("text"))).and_then(Value::as_str) {
                    self.snapshot_thinking(index, true, *summary_index, text, emit)?;
                }
            }
            TextStreamEventKind::FunctionCallArgumentsDelta { delta, .. } => self.calls.entry(index).or_default().arguments.push_str(delta),
            TextStreamEventKind::FunctionCallArgumentsDone { arguments, .. } => self.calls.entry(index).or_default().arguments = arguments.clone(),
            TextStreamEventKind::ResponseFailed { .. } => return Err("Grok reported a response failure. No pending tools were executed.".into()),
            TextStreamEventKind::ResponseIncomplete { .. } => return Err("Grok stopped with an incomplete response. No pending tools were executed.".into()),
            TextStreamEventKind::Passthrough { event_type } if matches!(event_type.as_str(), "error" | "response.error") => return Err("Grok reported a response failure. No pending tools were executed.".into()),
            TextStreamEventKind::ResponseCompleted { .. } => {
                let response = &value["response"];
                if response.get("error").is_some_and(|error| !error.is_null()) || response.get("status").and_then(Value::as_str).is_some_and(|status| status != "completed") {
                    return Err("Grok did not report successful response completion.".into());
                }
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for (index, item) in items.iter().enumerate() { self.item(index as u64, item, emit)?; }
                }
                self.prompt_tokens = response["usage"]["input_tokens"].as_u64();
                self.completion_tokens = response["usage"]["output_tokens"].as_u64();
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn into_turn(self, tools: &[ToolDefinition]) -> Result<AgentTurn, String> {
        let offered = tools.iter().map(|tool| tool.name.as_str()).collect::<HashSet<_>>();
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        for call in self.calls.into_values() {
            if call.id.is_empty() || !ids.insert(call.id.clone()) || !offered.contains(call.name.as_str()) {
                return Err("Grok returned a function call that does not match Taceta's offered tools. No pending tools were executed.".into());
            }
            let arguments: Value = serde_json::from_str(&call.arguments).map_err(|_| "Grok returned invalid function arguments. No pending tools were executed.")?;
            if !arguments.is_object() { return Err("Grok function arguments must be a JSON object. No pending tools were executed.".into()); }
            calls.push(AgentToolCall { id: call.id, name: call.name, arguments });
        }
        Ok(AgentTurn { content: self.text.into_values().collect::<String>(), tool_calls: calls,
            prompt_tokens: self.prompt_tokens, completion_tokens: self.completion_tokens })
    }
}

pub(super) async fn consume<S>(mut stream: S, tools: &[ToolDefinition], mut emit: impl FnMut(OutputDelta) -> Result<(), String>) -> Result<AgentTurn, String>
where S: Stream<Item = Result<ValidatedTextStreamEvent, GrokError>> + Unpin {
    let mut output = Output::default();
    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| error.to_string())?;
        if output.accept(&event, &mut emit)? { return output.into_turn(tools); }
    }
    Err("Grok upstream ended before producing a response.".into())
}
