use super::super::{
    ApprovalPolicy, ApprovalRequest, Control, Event, Question, QuestionOption, RunRequest, RunStatus,
    ToolItem, ToolStatus, Usage,
};
use crate::domain::{Attachment, AttachmentPayload};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

/// The turn's input: the prompt, then each attachment as its own item.
pub(super) fn input(prompt: &str, attachments: &[Attachment], images: bool) -> Result<Vec<Value>, String> {
    if prompt.trim().is_empty() && attachments.is_empty() { return Err("Enter a message or add an attachment.".into()); }
    let mut result = Vec::new();
    if !prompt.is_empty() { result.push(json!({"type":"text","text":prompt})); }
    for attachment in attachments {
        match &attachment.payload {
            AttachmentPayload::Text(content) => result.push(json!({
                "type":"text", "text":super::super::attached_text(attachment, content)
            })),
            AttachmentPayload::Image { media_type, base64 } => {
                if !images { return Err("The selected GPT model did not advertise image input support.".into()); }
                if !super::super::accepted_image_type(media_type) || base64.is_empty() {
                    return Err("GPT image attachment has an unsupported media type or no image data.".into());
                }
                result.push(json!({"type":"image","url":format!("data:{media_type};base64,{base64}")}));
            }
        }
    }
    Ok(result)
}

/// What a thread is started or resumed with. Asking every time keeps writes
/// inside the workspace and commands off the network; a conversation without
/// tools gets a read-only sandbox and nothing to act with; otherwise the
/// CLI's own configuration decides and nothing about permissions is sent.
/// `mcp_servers` names the MCP servers a conversation without tools turns off.
pub(super) fn thread_params(request: &RunRequest, cwd: &str, mcp_servers: &[String]) -> Value {
    let mut params = json!({"modelProvider":"openai", "cwd":cwd});
    if let Some(model) = &request.model { params["model"] = json!(model); }
    match request.approval {
        ApprovalPolicy::NoTools => {
            params["approvalPolicy"] = json!("never");
            params["approvalsReviewer"] = json!("user");
            params["sandbox"] = json!("read-only");
            params["environments"] = json!([]);
            params["config"] = json!({
                "web_search":"disabled", "features.apps":false, "features.plugins":false,
                "features.hooks":false, "features.plugin_hooks":false,
                "features.shell_tool":false, "features.multi_agent":false, "features.multi_agent_v2":false
            });
            // The read-only sandbox does not reach an MCP server, which runs as
            // its own process, so each one is turned off for this thread.
            for name in mcp_servers { params["config"][format!("mcp_servers.{name}.enabled")] = json!(false); }
        }
        ApprovalPolicy::AskEveryTime => {
            params["approvalPolicy"] = json!("untrusted");
            params["approvalsReviewer"] = json!("user");
            params["sandbox"] = json!("workspace-write");
            params["runtimeWorkspaceRoots"] = json!([cwd]);
            params["config"] = json!({
                "sandbox_workspace_write.network_access":false,
                "sandbox_workspace_write.exclude_tmpdir_env_var":true,
                "sandbox_workspace_write.exclude_slash_tmp":true
            });
        }
        ApprovalPolicy::FollowCli => params["runtimeWorkspaceRoots"] = json!([cwd]),
    }
    if let Some(thread_id) = &request.session_id { params["threadId"] = json!(thread_id); }
    else { params["ephemeral"] = json!(false); }
    params
}

/// The MCP servers in Codex's effective configuration, from a `config/read`
/// answer. A name that cannot be written as one segment of a dotted override
/// could not be turned off, so it stops the run.
pub(super) fn mcp_server_names(response: &Value) -> Result<Vec<String>, String> {
    let servers = match response.pointer("/config/mcp_servers") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(servers)) => servers,
        Some(_) => return Err("Codex reported its MCP servers in an unknown form. No work was started.".into()),
    };
    servers.keys().map(|name| {
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { Ok(name.clone()) }
        else { Err(format!("The Codex MCP server name {name:?} cannot be turned off for a conversation without tools. No work was started.")) }
    }).collect()
}

pub(super) fn turn_params(thread_id: &str, request: &RunRequest, cwd: &str, input: Vec<Value>) -> Value {
    let mut params = json!({"threadId":thread_id, "input":input, "cwd":cwd, "summary":"auto"});
    if let Some(model) = &request.model { params["model"] = json!(model); }
    if let Some(effort) = &request.effort { params["effort"] = json!(effort); }
    match request.approval {
        ApprovalPolicy::NoTools => {
            params["approvalPolicy"] = json!("never");
            params["approvalsReviewer"] = json!("user");
            params["sandboxPolicy"] = json!({"type":"readOnly","networkAccess":false});
            params["environments"] = json!([]);
        }
        ApprovalPolicy::AskEveryTime => {
            params["approvalPolicy"] = json!("untrusted");
            params["approvalsReviewer"] = json!("user");
            params["sandboxPolicy"] = json!({"type":"workspaceWrite","writableRoots":[cwd],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true});
        }
        ApprovalPolicy::FollowCli => {}
    }
    params
}

/// Refuses to run when Codex did not take the permissions Taceta asked for.
/// When the CLI's own configuration decides, there is nothing to compare.
pub(super) fn validate_effective_permissions(response: &Value, approval: ApprovalPolicy, cwd: &str) -> Result<(), String> {
    let (sandbox, policy, coding) = match approval {
        ApprovalPolicy::FollowCli => return Ok(()),
        ApprovalPolicy::NoTools => ("readOnly", "never", false),
        ApprovalPolicy::AskEveryTime => ("workspaceWrite", "untrusted", true),
    };
    if response.pointer("/sandbox/type").and_then(Value::as_str) != Some(sandbox)
        || response.get("approvalPolicy").and_then(Value::as_str) != Some(policy)
        || response.get("approvalsReviewer").and_then(Value::as_str) != Some("user") {
        return Err("Codex did not accept Taceta's sandbox and user-approval policy. No work was started.".into());
    }
    // A workspace-write sandbox must state that network access is off; an
    // absent or re-typed field is not treated as off.
    let network = response.pointer("/sandbox/networkAccess").and_then(Value::as_bool);
    if network == Some(true) || (coding && network != Some(false)) {
        return Err("Codex did not confirm that tool network access is off. No work was started.".into());
    }
    if coding {
        let policy = response.get("sandbox").ok_or("Codex did not return its effective sandbox.")?;
        if policy.get("excludeTmpdirEnvVar").and_then(Value::as_bool) != Some(true) || policy.get("excludeSlashTmp").and_then(Value::as_bool) != Some(true) {
            return Err("Codex did not apply Taceta's workspace write restrictions. No work was started.".into());
        }
        if policy.get("writableRoots").and_then(Value::as_array).is_some_and(|roots| roots.iter().any(|root| root.as_str() != Some(cwd))) {
            return Err("Codex granted writes outside the selected workspace. No work was started.".into());
        }
    }
    Ok(())
}

pub(super) enum Effect {
    Continue,
    Terminal(RunStatus, Option<String>),
}

enum PendingKind {
    Approval { accept_allowed: bool },
    Permissions(Value),
    Questions(Vec<String>),
    /// An MCP server's request for a plain yes or no.
    Confirmation,
}

struct Pending { wire_id: Value, kind: PendingKind }

/// What `server_request` leaves for the caller to do with the request.
pub(super) enum Asked {
    /// The user was asked; the answer comes back through [`RunState::control`].
    User,
    /// Answered here, with this result.
    Answered(Value),
}

pub(super) struct RunState {
    pub thread_id: String,
    pub turn_id: Option<String>,
    owned_threads: HashSet<String>,
    no_tools: bool,
    /// Answer items that have begun, and those whose text arrived in pieces.
    messages: HashSet<String>,
    streamed: HashSet<String>,
    thinking_items: HashSet<String>,
    tools: HashMap<String, ToolItem>,
    pending: HashMap<Uuid, Pending>,
    events: UnboundedSender<Event>,
}

impl RunState {
    pub fn new(thread_id: String, request: &RunRequest, events: UnboundedSender<Event>) -> Self {
        Self {
            owned_threads: HashSet::from([thread_id.clone()]), thread_id, turn_id: None,
            no_tools: request.approval == ApprovalPolicy::NoTools,
            messages: HashSet::new(), streamed: HashSet::new(), thinking_items: HashSet::new(),
            tools: HashMap::new(), pending: HashMap::new(), events,
        }
    }

    fn send(&self, event: Event) -> Result<(), String> {
        self.events.send(event).map_err(|_| "The view of this run was closed.".into())
    }

    /// A turn can hold several answer items; each starts on a line of its own.
    fn message_started(&mut self, id: &str) -> Result<(), String> {
        if self.messages.insert(id.into()) && self.messages.len() > 1 { self.send(Event::MessageDelta("\n\n".into()))?; }
        Ok(())
    }

    pub fn set_turn(&mut self, turn: &Value) -> Result<(), String> {
        let id = required_string(turn, "id")?;
        if self.turn_id.as_deref().is_some_and(|current| current != id) {
            return Err("Codex changed the active turn unexpectedly.".into());
        }
        self.turn_id = Some(id.into());
        Ok(())
    }

    pub fn notification(&mut self, method: &str, params: &Value) -> Result<Effect, String> {
        if method=="thread/started" {
            if let Some(thread)=params.get("thread") {
                if thread.get("parentThreadId").and_then(Value::as_str).is_some_and(|parent|self.owned_threads.contains(parent)) {
                    if let Some(id)=thread.get("id").and_then(Value::as_str) {self.owned_threads.insert(id.into());}
                }
            }
            return Ok(Effect::Continue);
        }
        let source=params.get("threadId").and_then(Value::as_str);
        if source.is_some_and(|id| !self.owned_threads.contains(id)) {return Ok(Effect::Continue);}
        if source.is_some_and(|id|id!=self.thread_id) && matches!(method,"turn/started"|"turn/completed") {return Ok(Effect::Continue);}
        match method {
            "turn/started" => self.set_turn(params.get("turn").ok_or("Codex turn event has no turn.")?)?,
            "turn/completed" => {
                let turn = params.get("turn").ok_or("Codex completion has no turn.")?;
                self.set_turn(turn)?;
                // Completed items are authoritative, including when a server emitted no deltas.
                if let Some(items) = turn.get("items").and_then(Value::as_array) {
                    for item in items { self.item(item, true)?; }
                }
                self.resolve_all();
                return Ok(match required_string(turn, "status")? {
                    "completed" => Effect::Terminal(RunStatus::Completed, None),
                    "interrupted" => Effect::Terminal(RunStatus::Interrupted, Some("Codex interrupted this turn.".into())),
                    "failed" => Effect::Terminal(RunStatus::Failed, Some(turn.pointer("/error/message").and_then(Value::as_str).unwrap_or("Codex turn failed.").into())),
                    _ => return Err("Codex emitted an unknown terminal turn status.".into()),
                });
            }
            "item/started" | "item/completed" => {
                let item = params.get("item").ok_or("Codex item event has no item.")?;
                self.item(item, method == "item/completed")?;
            }
            "item/agentMessage/delta" => {
                let id = required_string(params, "itemId")?;
                self.message_started(id)?;
                self.streamed.insert(id.into());
                self.send(Event::MessageDelta(required_string(params, "delta")?.into()))?;
            }
            "item/reasoning/summaryTextDelta" => {
                self.thinking_items.insert(required_string(params, "itemId")?.into());
                self.send(Event::ThinkingDelta(required_string(params, "delta")?.into()))?;
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let id = required_string(params, "itemId")?;
                let delta = required_string(params, "delta")?;
                let tool = self.tools.entry(id.into()).or_insert_with(|| ToolItem { id:id.into(), title:"Codex tool".into(), detail:String::new(), diff:None, status:ToolStatus::Running });
                tool.detail.push_str(delta);
                let event = Event::Tool(tool.clone());
                self.send(event)?;
            }
            "thread/tokenUsage/updated" => {
                let usage = params.get("tokenUsage").ok_or("Codex usage event has no usage.")?;
                let total = usage.get("total").ok_or("Codex usage event has no total.")?;
                self.send(Event::Usage(Usage {
                    input_tokens:total.get("inputTokens").and_then(Value::as_u64).ok_or("Invalid Codex input token count.")?,
                    output_tokens:total.get("outputTokens").and_then(Value::as_u64).ok_or("Invalid Codex output token count.")?,
                    context_window:usage.get("modelContextWindow").and_then(Value::as_u64),
                }))?;
            }
            "serverRequest/resolved" => {
                if let Some(wire_id) = params.get("requestId") {
                    let local = self.pending.iter().find_map(|(id,pending)| (&pending.wire_id == wire_id).then_some(*id));
                    if let Some(id) = local { self.pending.remove(&id); self.send(Event::ApprovalResolved { id })?; }
                }
            }
            "error" => {
                if params.get("willRetry").and_then(Value::as_bool) != Some(true) {
                    return Err(params.pointer("/error/message").or_else(||params.get("message")).and_then(Value::as_str).unwrap_or("Codex reported an error.").into());
                }
            }
            _ => {}
        }
        Ok(Effect::Continue)
    }

    fn item(&mut self, item: &Value, completed: bool) -> Result<(), String> {
        let id = required_string(item, "id")?;
        let kind = required_string(item, "type")?;
        match kind {
            "agentMessage" => {
                self.message_started(id)?;
                // The text is sent whole only when none of it arrived in pieces.
                if completed && self.streamed.insert(id.into()) { self.send(Event::MessageDelta(required_string(item, "text")?.into()))?; }
            }
            "reasoning" => {
                if completed && !self.thinking_items.contains(id) {
                    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                        let text = summary.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
                        if !text.is_empty() { self.thinking_items.insert(id.into()); self.send(Event::ThinkingDelta(text))?; }
                    }
                }
            }
            "userMessage" | "hookPrompt" | "functionCallOutput" | "enteredReviewMode" | "exitedReviewMode" | "contextCompaction" | "plan" => {}
            _ => {
                if matches!(kind,"collabAgentToolCall"|"collabToolCall") {
                    if let Some(receivers)=item.get("receiverThreadIds").and_then(Value::as_array) {
                        for receiver in receivers.iter().filter_map(Value::as_str) {self.owned_threads.insert(receiver.into());}
                    }
                    if let Some(receiver)=item.get("receiverThreadId").or_else(||item.get("newThreadId")).and_then(Value::as_str) {self.owned_threads.insert(receiver.into());}
                }
                let previous_output = self.tools.get(id).map(|tool| tool.detail.as_str()).unwrap_or("");
                let (title, detail, diff) = tool_display(item, previous_output);
                let tool = ToolItem { id:id.into(), title, detail, diff, status:tool_status(item, completed) };
                self.tools.insert(id.into(), tool.clone());
                self.send(Event::Tool(tool))?;
                if self.no_tools && matches!(kind, "commandExecution" | "fileChange" | "mcpToolCall" | "dynamicToolCall" | "webSearch" | "imageView" | "imageGeneration" | "collabAgentToolCall") {
                    return Err(format!("Codex attempted {kind} in a conversation without tools; the turn was stopped."));
                }
            }
        }
        Ok(())
    }

    pub fn server_request(&mut self, wire_id: Value, method: &str, params: &Value) -> Result<Asked, String> {
        let request_thread=required_string(params,"threadId")?;
        if !self.owned_threads.contains(request_thread) {
            return Err("Codex requested an action for an unowned thread.".into());
        }
        if self.pending.values().any(|pending| pending.wire_id == wire_id) { return Err("Codex repeated a pending request ID.".into()); }
        if let Some(turn_id) = params.get("turnId").and_then(Value::as_str).filter(|_|request_thread==self.thread_id) {
            self.set_turn(&json!({"id":turn_id}))?;
        }
        let id = Uuid::new_v4();
        let kind = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                if self.no_tools { return Err("Codex requested execution approval in a conversation without tools.".into()); }
                let item_id = required_string(params,"itemId")?;
                let mut detail = self.tools.get(item_id).map(|tool| match &tool.diff {
                    Some(diff) if tool.detail.is_empty() => diff.clone(),
                    Some(diff) => format!("{diff}\n{}", tool.detail),
                    None => tool.detail.clone(),
                }).unwrap_or_default();
                for (key, label) in [("command","Command"),("cwd","Directory"),("reason","Reason"),("grantRoot","Requested write root")] {
                    if let Some(text) = params.get(key).and_then(Value::as_str) { detail.push_str(&format!("\n{label}: {text}")); }
                }
                if let Some(permissions) = params.get("additionalPermissions").filter(|value|!value.is_null()) {
                    detail.push_str(&format!("\nAdditional permissions: {permissions}"));
                }
                let accept_allowed = params.get("availableDecisions").and_then(Value::as_array)
                    .is_none_or(|values|values.iter().any(|value|value.as_str()==Some("accept")));
                self.send(Event::Approval(ApprovalRequest { id, title:if method.contains("commandExecution") {"Codex requests permission to run a command."} else {"Codex requests permission to change files."}.into(), detail }))?;
                PendingKind::Approval { accept_allowed }
            }
            "item/permissions/requestApproval" => {
                if self.no_tools { return Err("Codex requested additional permissions in a conversation without tools.".into()); }
                let permissions = params.get("permissions").filter(|value|value.is_object()).ok_or("Codex permission request has no permission details.")?.clone();
                self.send(Event::Approval(ApprovalRequest {
                    id, title:"Codex requests these additional permissions for this turn.".into(),
                    detail:format!("{}\n{}", params.get("reason").and_then(Value::as_str).unwrap_or(""), serde_json::to_string_pretty(&permissions).map_err(|error|error.to_string())?),
                }))?;
                PendingKind::Permissions(permissions)
            }
            "item/tool/requestUserInput" => {
                let values = params.get("questions").and_then(Value::as_array).ok_or("Codex question request has no questions.")?;
                let mut ids = HashSet::new();
                let mut questions = Vec::new();
                for value in values {
                    let question_id = required_string(value,"id")?.to_owned();
                    if !ids.insert(question_id.clone()) { return Err("Codex repeated a question identifier.".into()); }
                    let options = value.get("options").and_then(Value::as_array).map(|options| options.iter().map(|option| Ok(QuestionOption {
                        label:required_string(option,"label")?.into(), description:required_string(option,"description")?.into()
                    })).collect::<Result<Vec<_>,String>>()).transpose()?.unwrap_or_default();
                    questions.push(Question { id:question_id, header:required_string(value,"header")?.into(), question:required_string(value,"question")?.into(), options, is_secret:value.get("isSecret").and_then(Value::as_bool).unwrap_or(false) });
                }
                if questions.is_empty() { return Err("Codex returned an empty question request.".into()); }
                let question_ids = questions.iter().map(|question|question.id.clone()).collect();
                self.send(Event::Questions { id, questions })?;
                PendingKind::Questions(question_ids)
            }
            // An MCP server asks the user something. A plain yes or no is put
            // to the user; a form or a page to open cannot be answered here
            // and is declined, and the turn goes on.
            "mcpServer/elicitation/request" => {
                let plain = !self.no_tools
                    && params.get("mode").and_then(Value::as_str) == Some("form")
                    && params.pointer("/requestedSchema/properties").and_then(Value::as_object).is_some_and(Map::is_empty);
                if !plain { return Ok(Asked::Answered(json!({"action":"decline"}))); }
                self.send(Event::Approval(ApprovalRequest {
                    id, title:format!("The MCP server {} asks for confirmation.", params.get("serverName").and_then(Value::as_str).unwrap_or("")),
                    detail:required_string(params,"message")?.into(),
                }))?;
                PendingKind::Confirmation
            }
            _ => return Err(format!("Taceta does not support the Codex request '{method}'. The turn was stopped.")),
        };
        self.pending.insert(id, Pending { wire_id, kind });
        Ok(Asked::User)
    }

    /// Turns the user's decision into the answer for the request it belongs
    /// to: Taceta's id, the wire id, and the result to send.
    pub fn control(&mut self, control: Control) -> Result<Option<(Uuid, Value, Value)>, String> {
        let (id, payload) = match control {
            Control::Approval { id, approved } => {
                let Some(pending) = self.pending.get(&id) else { return Ok(None); };
                let response = match &pending.kind {
                    PendingKind::Approval { accept_allowed } => {
                        if approved && !accept_allowed { return Err("Codex does not permit single-action approval for this request.".into()); }
                        json!({"decision":if approved {"accept"} else {"decline"}})
                    }
                    PendingKind::Permissions(permissions) => json!({"permissions":if approved {permissions.clone()} else {json!({})},"scope":"turn"}),
                    PendingKind::Confirmation => if approved { json!({"action":"accept","content":{}}) } else { json!({"action":"decline"}) },
                    PendingKind::Questions(_) => return Err("An approval response cannot answer Codex questions.".into()),
                };
                (id,response)
            }
            Control::Answers { id, answers } => {
                let Some(pending) = self.pending.get(&id) else { return Ok(None); };
                let PendingKind::Questions(expected) = &pending.kind else { return Err("A question response cannot approve a Codex action.".into()); };
                let mut mapped = Map::new();
                for (question, answer) in answers {
                    if !expected.contains(&question) || mapped.contains_key(&question) { return Err("The Codex question response contains an unknown or duplicate question.".into()); }
                    mapped.insert(question,json!({"answers":[answer]}));
                }
                if mapped.len()!=expected.len() { return Err("Answer each Codex question before continuing.".into()); }
                (id,json!({"answers":mapped}))
            }
        };
        let pending = self.pending.remove(&id).expect("pending request was checked above");
        Ok(Some((id,pending.wire_id,payload)))
    }

    pub fn resolve_all(&mut self) {
        for id in self.pending.drain().map(|(id,_)|id) { let _ = self.events.send(Event::ApprovalResolved { id }); }
    }
}

pub(super) fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value.get(field).and_then(Value::as_str).ok_or_else(||format!("Codex response is missing '{field}'."))
}

fn tool_status(item: &Value, completed: bool) -> ToolStatus {
    let refused = matches!(item.get("status").and_then(Value::as_str), Some("failed" | "declined"));
    let exit = item.get("exitCode").and_then(Value::as_i64);
    if refused || exit.is_some_and(|code| code != 0) { ToolStatus::Failed }
    else if completed { ToolStatus::Completed }
    else { ToolStatus::Running }
}

/// A unified diff with its changed lines marked the way the view colors
/// them: `+ ` and `- `.
fn marked_diff(diff: &str) -> String {
    diff.lines().map(|line| {
        if line.starts_with("+++") || line.starts_with("---") { line.to_owned() }
        else if let Some(added) = line.strip_prefix('+') { format!("+ {added}") }
        else if let Some(removed) = line.strip_prefix('-') { format!("- {removed}") }
        else { line.to_owned() }
    }).collect::<Vec<_>>().join("\n")
}

/// The title, the plain detail and the diff shown for a tool item.
fn tool_display(item: &Value, previous: &str) -> (String, String, Option<String>) {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("tool");
    match kind {
        "commandExecution" => {
            let command = item.get("command").and_then(Value::as_str).unwrap_or("Command");
            let output = item.get("aggregatedOutput").and_then(Value::as_str);
            let detail = if let Some(output) = output {
                format!("{}\n{}\n{}\nexit: {}",command,item.get("cwd").and_then(Value::as_str).unwrap_or(""),output,item.get("exitCode").map(Value::to_string).unwrap_or_default())
            } else if !previous.is_empty() { previous.into() }
            else { format!("{}\n{}\n",command,item.get("cwd").and_then(Value::as_str).unwrap_or("")) };
            (command.into(),detail,None)
        }
        "fileChange" => {
            let changes = item.get("changes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            let paths = changes.iter().filter_map(|change| change.get("path").and_then(Value::as_str)).collect::<Vec<_>>();
            let diff = changes.iter().map(|change| format!("{}\n{}",change.get("path").and_then(Value::as_str).unwrap_or(""),marked_diff(change.get("diff").and_then(Value::as_str).unwrap_or("")))).collect::<Vec<_>>().join("\n");
            (format!("Edit {}", paths.join(", ")), previous.into(), (!diff.is_empty()).then_some(diff))
        }
        _ => {
            let name = item.get("tool").and_then(Value::as_str).unwrap_or(kind);
            let detail = item.get("query").or_else(||item.get("path")).or_else(||item.get("arguments")).map(|value|value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())).unwrap_or_else(||previous.into());
            (name.into(),detail,None)
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::domain::Attachment;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    pub(in super::super) fn request(approval: ApprovalPolicy) -> RunRequest {
        RunRequest {
            session_id: None, workspace: PathBuf::from("/workspace"), model: Some("model".into()), effort: None,
            prompt: "work".into(), attachments: Vec::new(), approval, max_duration_secs: 30,
        }
    }

    #[test]
    fn the_input_is_the_prompt_and_its_attachments_and_nothing_else() {
        let attachments = vec![Attachment {name:"notes.txt".into(),payload:AttachmentPayload::Text("document".into())},Attachment{name:"photo.png".into(),payload:AttachmentPayload::Image{media_type:"image/png".into(),base64:"aW1hZ2U=".into()}}];
        let wire = input("current prompt",&attachments,true).unwrap();
        assert_eq!(wire[0],json!({"type":"text","text":"current prompt"}));
        assert!(wire[1]["text"].as_str().unwrap().contains("document"));
        assert_eq!(wire[2]["url"],"data:image/png;base64,aW1hZ2U=");
        assert!(input("current prompt",&attachments,false).is_err());
        let params = turn_params("thread",&request(ApprovalPolicy::AskEveryTime),"/workspace",wire);
        assert!(params.get("messages").is_none());
        assert!(params.get("history").is_none());
        assert_eq!(params["approvalPolicy"],"untrusted");
        assert_eq!(params["sandboxPolicy"]["networkAccess"],false);
    }

    #[test]
    fn each_approval_policy_asks_codex_for_its_own_permissions() {
        let ask = thread_params(&request(ApprovalPolicy::AskEveryTime),"/workspace",&["node_repl".into()]);
        assert_eq!(ask["approvalPolicy"],"untrusted");
        assert_eq!(ask["sandbox"],"workspace-write");
        assert_eq!(ask["config"]["sandbox_workspace_write.network_access"],false);
        // The CLI's own configuration decides: nothing about permissions is sent.
        for params in [thread_params(&request(ApprovalPolicy::FollowCli),"/workspace",&["node_repl".into()]), turn_params("thread",&request(ApprovalPolicy::FollowCli),"/workspace",vec![])] {
            for key in ["approvalPolicy","approvalsReviewer","sandbox","sandboxPolicy","config"] { assert!(params.get(key).is_none(),"{key}"); }
        }
        let chat = request(ApprovalPolicy::NoTools);
        let thread = thread_params(&chat,"/private/chat",&["node_repl".into(),"cloudflare-api".into()]);
        let turn = turn_params("thread",&chat,"/private/chat",vec![]);
        assert_eq!(thread["environments"],json!([]));
        assert_eq!(thread["sandbox"],"read-only");
        assert_eq!(thread["config"]["features.shell_tool"],false);
        assert_eq!(thread["config"]["mcp_servers.node_repl.enabled"],false);
        assert_eq!(thread["config"]["mcp_servers.cloudflare-api.enabled"],false);
        // Only a conversation without tools turns MCP servers off.
        assert!(ask["config"].get("mcp_servers.node_repl.enabled").is_none());
        assert_eq!(turn["environments"],json!([]));
        assert_eq!(turn["approvalPolicy"],"never");
        assert_eq!(turn["sandboxPolicy"],json!({"type":"readOnly","networkAccess":false}));
    }

    #[test]
    fn every_mcp_server_in_the_effective_config_is_named_and_an_unwritable_name_stops_the_run() {
        let names = mcp_server_names(&json!({"config":{"mcp_servers":{"node_repl":{"command":"node"},"cloudflare-api":{"url":"https://example.invalid"}}}})).unwrap();
        assert_eq!(names.len(),2);
        assert!(names.contains(&"node_repl".to_owned()) && names.contains(&"cloudflare-api".to_owned()));
        assert!(mcp_server_names(&json!({"config":{}})).unwrap().is_empty());
        assert!(mcp_server_names(&json!({"config":{"mcp_servers":[]}})).is_err());
        assert!(mcp_server_names(&json!({"config":{"mcp_servers":{"a.b":{}}}})).is_err());
    }

    #[test]
    fn permissions_codex_did_not_take_stop_the_run_before_a_turn() {
        let mut response=json!({"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite","writableRoots":["/workspace"],"networkAccess":false,"excludeSlashTmp":true,"excludeTmpdirEnvVar":true}});
        let ask = ApprovalPolicy::AskEveryTime;
        assert!(validate_effective_permissions(&response,ask,"/workspace").is_ok());
        response["sandbox"]["networkAccess"]=json!(true);
        assert!(validate_effective_permissions(&response,ask,"/workspace").is_err());
        // The CLI's own configuration is not second-guessed.
        assert!(validate_effective_permissions(&response,ApprovalPolicy::FollowCli,"/workspace").is_ok());
        for unconfirmed in [json!("false"),Value::Null] {
            response["sandbox"]["networkAccess"]=unconfirmed;
            assert!(validate_effective_permissions(&response,ask,"/workspace").is_err());
        }
        response["sandbox"].as_object_mut().unwrap().remove("networkAccess");
        assert!(validate_effective_permissions(&response,ask,"/workspace").is_err());
        response["sandbox"]["networkAccess"]=json!(false);response["approvalsReviewer"]=json!("auto_review");
        assert!(validate_effective_permissions(&response,ask,"/workspace").is_err());
    }

    #[test]
    fn a_turn_that_failed_or_was_interrupted_is_never_reported_as_completed() {
        for (wire,expected) in [("completed",RunStatus::Completed),("failed",RunStatus::Failed),("interrupted",RunStatus::Interrupted)] {
            let (tx,_rx)=mpsc::unbounded_channel(); let mut state=RunState::new("thread".into(),&request(ApprovalPolicy::AskEveryTime),tx);
            assert!(matches!(state.notification("turn/completed",&json!({"threadId":"thread","turn":{"id":"turn","status":wire,"items":[],"error":{"message":"failure"}}})).unwrap(),Effect::Terminal(status,_) if status==expected));
        }
    }

    #[test]
    fn an_answer_goes_back_under_the_request_it_belongs_to_exactly_once() {
        let (tx,mut rx)=mpsc::unbounded_channel();let mut state=RunState::new("thread".into(),&request(ApprovalPolicy::AskEveryTime),tx);
        assert!(matches!(state.server_request(json!(17),"item/commandExecution/requestApproval",&json!({"threadId":"thread","turnId":"turn","itemId":"cmd","command":"cargo test","cwd":"/workspace"})).unwrap(),Asked::User));
        let Event::Approval(approval)=rx.try_recv().unwrap() else {panic!("approval missing")};
        assert!(approval.detail.contains("Command: cargo test"));
        let (_,wire,payload)=state.control(Control::Approval{id:approval.id,approved:false}).unwrap().unwrap();
        assert_eq!(wire,json!(17));assert_eq!(payload,json!({"decision":"decline"}));
        assert!(state.control(Control::Approval{id:approval.id,approved:true}).unwrap().is_none());
        state.server_request(json!("question-wire"),"item/tool/requestUserInput",&json!({"threadId":"thread","turnId":"turn","questions":[{"id":"q1","header":"Choice","question":"Which?","options":[]}]})).unwrap();
        let Event::Questions{id,..}=rx.try_recv().unwrap() else {panic!("questions missing")};
        assert!(state.control(Control::Approval{id,approved:true}).is_err());
        let (_,wire,payload)=state.control(Control::Answers{id,answers:vec![("q1".into(),"A".into())]}).unwrap().unwrap();
        assert_eq!(wire,"question-wire");assert_eq!(payload,json!({"answers":{"q1":{"answers":["A"]}}}));
    }

    #[test]
    fn an_mcp_server_gets_a_yes_or_no_from_the_user_and_a_refusal_for_anything_else() {
        let (tx,mut rx)=mpsc::unbounded_channel();let mut state=RunState::new("thread".into(),&request(ApprovalPolicy::AskEveryTime),tx);
        let form=json!({"threadId":"thread","serverName":"files","mode":"form","message":"Fill this in","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}}}});
        assert!(matches!(state.server_request(json!(1),"mcpServer/elicitation/request",&form).unwrap(),Asked::Answered(answer) if answer==json!({"action":"decline"})));
        assert!(rx.try_recv().is_err());
        let plain=json!({"threadId":"thread","serverName":"files","mode":"form","message":"Delete the cache?","requestedSchema":{"type":"object","properties":{}}});
        assert!(matches!(state.server_request(json!(2),"mcpServer/elicitation/request",&plain).unwrap(),Asked::User));
        let Event::Approval(asked)=rx.try_recv().unwrap() else {panic!("confirmation missing")};
        assert_eq!(asked.detail,"Delete the cache?");
        let (_,wire,payload)=state.control(Control::Approval{id:asked.id,approved:true}).unwrap().unwrap();
        assert_eq!(wire,json!(2));assert_eq!(payload,json!({"action":"accept","content":{}}));
        assert!(state.server_request(json!(3),"unknown/request",&json!({"threadId":"thread"})).is_err());
    }

    #[test]
    fn answer_text_arrives_once_and_separate_answers_start_on_their_own_line() {
        let (tx,mut rx)=mpsc::unbounded_channel();
        let mut state=RunState::new("thread".into(),&request(ApprovalPolicy::AskEveryTime),tx);
        state.notification("item/agentMessage/delta",&json!({"threadId":"thread","turnId":"turn","itemId":"m","delta":"draft"})).unwrap();
        // The whole text of an answer that was streamed is not sent again.
        state.notification("item/completed",&json!({"threadId":"thread","turnId":"turn","item":{"type":"agentMessage","id":"m","text":"draft"}})).unwrap();
        // An answer that was never streamed arrives whole.
        state.notification("item/completed",&json!({"threadId":"thread","turnId":"turn","item":{"type":"agentMessage","id":"n","text":"final"}})).unwrap();
        let mut text=String::new();
        while let Ok(event)=rx.try_recv() { if let Event::MessageDelta(delta)=event { text.push_str(&delta); } }
        assert_eq!(text,"draft\n\nfinal");
    }

    #[test]
    fn a_file_change_is_shown_as_a_diff_and_a_failed_command_as_failed() {
        let (tx,mut rx)=mpsc::unbounded_channel();
        let mut state=RunState::new("thread".into(),&request(ApprovalPolicy::AskEveryTime),tx);
        state.notification("item/completed",&json!({"threadId":"thread","item":{"type":"fileChange","id":"f","status":"completed","changes":[{"path":"/workspace/a.txt","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-old\n+new"}]}})).unwrap();
        let Event::Tool(edit)=rx.try_recv().unwrap() else {panic!("edit missing")};
        assert_eq!(edit.title,"Edit /workspace/a.txt");
        assert_eq!(edit.diff.as_deref(),Some("/workspace/a.txt\n@@ -1 +1 @@\n- old\n+ new"));
        assert_eq!(edit.status,ToolStatus::Completed);
        state.notification("item/completed",&json!({"threadId":"thread","item":{"type":"commandExecution","id":"c","command":"false","cwd":"/workspace","aggregatedOutput":"","exitCode":1,"status":"failed"}})).unwrap();
        let Event::Tool(command)=rx.try_recv().unwrap() else {panic!("command missing")};
        assert_eq!(command.status,ToolStatus::Failed);
    }

    #[test]
    fn a_conversation_without_tools_stops_at_the_first_action_or_approval() {
        let (tx,_rx)=mpsc::unbounded_channel();let mut state=RunState::new("thread".into(),&request(ApprovalPolicy::NoTools),tx);
        assert!(state.server_request(json!(1),"item/commandExecution/requestApproval",&json!({"threadId":"thread","turnId":"turn","itemId":"item","command":"touch file"})).is_err());
        assert!(state.notification("item/started",&json!({"threadId":"thread","item":{"type":"commandExecution","id":"cmd","command":"ls","cwd":"/"}})).is_err());
    }
}
