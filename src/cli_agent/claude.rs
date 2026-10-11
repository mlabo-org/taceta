//! The driver for the official Claude Code CLI (`claude`).
//!
//! The CLI is started once per prompt with its stream-json protocol on stdio.
//! This file turns that protocol into the types in [`super`]. Nothing outside
//! it sees the CLI's message shapes. Taceta performs no sign-in: the user
//! signs in with the CLI itself, and no credential passes through here.

use super::{
    ApprovalPolicy, ApprovalRequest, CliAgent, CliFuture, Control, Event, ModelOption, Readiness,
    RunOutcome, RunRequest, RunStatus, ToolItem, ToolStatus, Usage,
    process::{CliProcess, find_executable, usual_install_dirs},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
    time::{Instant, sleep_until, timeout},
};
use uuid::Uuid;

/// How long the CLI gets to answer an interrupt before it is shut down anyway.
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);
/// Characters of a tool's output kept in a [`ToolItem`].
const TOOL_RESULT_LIMIT: usize = 4_000;
/// Characters of a rendered diff kept for display and approval.
const DIFF_LIMIT: usize = 20_000;
/// Characters of a compact rendering of a tool's input.
const INPUT_LIMIT: usize = 2_000;
/// How long `--version` and `auth status` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const DENIED_MESSAGE: &str = "The user denied this action in Taceta.";
const UNSUPPORTED_REQUEST: &str = "Taceta does not support this request.";
const NOT_FOUND: &str = "The Claude Code CLI (`claude`) was not found. Install it and sign in \
                         with it yourself; Taceta does not handle the sign-in.";

const MODEL_ALIASES: [(&str, &str); 4] =
    [("fable", "Fable"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCode;

impl CliAgent for ClaudeCode {
    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn readiness(&self) -> CliFuture<Result<Readiness, String>> {
        Box::pin(async {
            let executable =
                find_executable("claude", &usual_install_dirs()).ok_or_else(|| NOT_FOUND.to_owned())?;
            let version = probe(&executable, &["--version"]).await.and_then(|output| {
                let text = String::from_utf8_lossy(&output).into_owned();
                text.lines().next().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned)
            });
            // Only the one boolean is read from this JSON; the rest of it
            // describes the account and is never kept.
            let signed_in = probe(&executable, &["auth", "status"])
                .await
                .and_then(|output| serde_json::from_slice::<Value>(&output).ok())
                .and_then(|status| status.get("loggedIn").and_then(Value::as_bool));
            Ok(Readiness { executable, version, signed_in })
        })
    }

    fn models(&self) -> CliFuture<Result<Vec<ModelOption>, String>> {
        Box::pin(async {
            Ok(MODEL_ALIASES
                .iter()
                .map(|(id, label)| ModelOption {
                    id: (*id).to_owned(),
                    label: (*label).to_owned(),
                    context_window: None,
                    efforts: EFFORTS.iter().map(|effort| (*effort).to_owned()).collect(),
                    default_effort: None,
                })
                .collect())
        })
    }

    fn run(
        &self,
        request: RunRequest,
        events: mpsc::UnboundedSender<Event>,
        controls: mpsc::UnboundedReceiver<Control>,
        cancel: watch::Receiver<bool>,
    ) -> CliFuture<RunOutcome> {
        Box::pin(async move {
            let Some(executable) = find_executable("claude", &usual_install_dirs()) else {
                return RunOutcome::failed(request.session_id, NOT_FOUND);
            };
            let process =
                match CliProcess::spawn(&executable, &build_args(&request), &request.workspace) {
                    Ok(process) => process,
                    Err(error) => return RunOutcome::failed(request.session_id, error),
                };
            let limits =
                Limits { duration: Duration::from_secs(request.max_duration_secs), grace: INTERRUPT_GRACE };
            drive(process, request, events, controls, cancel, limits).await
        })
    }
}

/// Runs a short, inference-free subcommand and returns its standard output.
async fn probe(executable: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(PROBE_TIMEOUT, command.output()).await.ok()?.ok()?;
    Some(output.stdout)
}

const NO_TOOLS_NOTE: &str = "This is a conversation without tools: no tool is available in this session. Answer from the conversation itself and say so when something would need a tool.";

/// The arguments that start one stream-json turn. The prompt itself goes over
/// stdin, so it never appears in the process list.
fn build_args(request: &RunRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    match request.approval {
        ApprovalPolicy::AskEveryTime => args.extend(["--permission-mode", "manual"].map(OsString::from)),
        ApprovalPolicy::FollowCli => {}
        // No built-in tools and no MCP servers. Manual mode stays on so that
        // anything that still asked would come to Taceta, which refuses it.
        ApprovalPolicy::NoTools => args.extend(
            ["--permission-mode", "manual", "--tools", "", "--strict-mcp-config", "--append-system-prompt", NO_TOOLS_NOTE]
                .map(OsString::from),
        ),
    }
    if let Some(model) = &request.model {
        args.extend([OsString::from("--model"), OsString::from(model)]);
    }
    if let Some(effort) = &request.effort {
        args.extend([OsString::from("--effort"), OsString::from(effort)]);
    }
    if let Some(session_id) = &request.session_id {
        args.extend([OsString::from("--resume"), OsString::from(session_id)]);
    }
    args
}

#[derive(Clone, Copy)]
struct Limits {
    /// The run's total time, including time waiting for an approval.
    duration: Duration,
    /// How long an interrupted CLI gets to end its turn.
    grace: Duration,
}

/// Plays one turn against a started CLI. The process stays owned by this
/// future, so dropping the future ends the CLI.
async fn drive(
    mut process: CliProcess,
    request: RunRequest,
    events: mpsc::UnboundedSender<Event>,
    controls: mpsc::UnboundedReceiver<Control>,
    cancel: watch::Receiver<bool>,
    limits: Limits,
) -> RunOutcome {
    let deadline = Instant::now() + limits.duration;
    let prompt = json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": request.prompt}]},
    });
    if let Err(error) = process.send(&prompt).await {
        process.shutdown().await;
        return RunOutcome::failed(request.session_id, error);
    }
    Turn {
        process,
        events,
        session_id: request.session_id,
        session_announced: false,
        tools: HashMap::new(),
        pending: HashMap::new(),
        stopping: None,
        events_gone: false,
        controls_open: true,
        grace: limits.grace,
    }
    .play(controls, cancel, deadline)
    .await
}

/// A permission request the CLI is waiting on.
struct Pending {
    /// The CLI's own request id.
    request_id: String,
    /// The tool input exactly as the CLI sent it; an approval returns it.
    input: Value,
}

/// Taceta asked the CLI to stop; the turn's result is still awaited.
struct Stopping {
    status: RunStatus,
    grace_ends: Instant,
}

struct TurnResult {
    outcome: Result<(), String>,
    usage: Option<Usage>,
}

enum Flow {
    Continue,
    Result(TurnResult),
}

struct Turn {
    process: CliProcess,
    events: mpsc::UnboundedSender<Event>,
    /// The id from the CLI's init message, else the one that was requested.
    session_id: Option<String>,
    session_announced: bool,
    tools: HashMap<String, ToolItem>,
    pending: HashMap<Uuid, Pending>,
    stopping: Option<Stopping>,
    /// The receiver of events was dropped.
    events_gone: bool,
    controls_open: bool,
    grace: Duration,
}

impl Turn {
    async fn play(
        mut self,
        mut controls: mpsc::UnboundedReceiver<Control>,
        mut cancel: watch::Receiver<bool>,
        deadline: Instant,
    ) -> RunOutcome {
        let mut cancel_open = true;
        if *cancel.borrow() {
            self.begin_stop(RunStatus::Interrupted).await;
        }
        loop {
            if self.events_gone {
                self.begin_stop(RunStatus::Interrupted).await;
            }
            let running = self.stopping.is_none();
            let grace_ends = self.stopping.as_ref().map_or(deadline, |stopping| stopping.grace_ends);
            tokio::select! {
                message = self.process.next() => match message {
                    Some(Ok(message)) => match self.handle(&message).await {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Result(result)) => return self.finish_turn(result).await,
                        Err(error) => return self.finish_failed(error).await,
                    },
                    Some(Err(error)) => return self.finish_failed(error).await,
                    None => {
                        return self
                            .finish_failed("Claude Code ended before finishing the turn.".to_owned())
                            .await;
                    }
                },
                _ = sleep_until(deadline), if running => {
                    self.begin_stop(RunStatus::LimitReached).await;
                }
                changed = cancel.changed(), if cancel_open && running => match changed {
                    Ok(()) => {
                        if *cancel.borrow_and_update() {
                            self.begin_stop(RunStatus::Interrupted).await;
                        }
                    }
                    // The sender went away; that is not a request to stop.
                    Err(_) => cancel_open = false,
                },
                control = controls.recv(), if self.controls_open => match control {
                    Some(control) => {
                        if let Err(error) = self.answer(control).await {
                            return self.finish_failed(error).await;
                        }
                    }
                    None => {
                        self.controls_open = false;
                        if !self.pending.is_empty() {
                            self.begin_stop(RunStatus::Interrupted).await;
                        }
                    }
                },
                _ = self.events.closed(), if running => {
                    self.begin_stop(RunStatus::Interrupted).await;
                }
                _ = sleep_until(grace_ends), if !running => {
                    let status = self.stopping.as_ref().map(|stopping| stopping.status);
                    return self.finish(status.unwrap_or(RunStatus::Interrupted), None).await;
                }
            }
        }
    }

    fn emit(&mut self, event: Event) {
        if self.events.send(event).is_err() {
            self.events_gone = true;
        }
    }

    fn resolve_pending(&mut self) {
        let ids: Vec<Uuid> = self.pending.drain().map(|(id, _)| id).collect();
        for id in ids {
            self.emit(Event::ApprovalResolved { id });
        }
    }

    /// Asks the CLI to end its turn. The first reason to stop is the one kept.
    async fn begin_stop(&mut self, status: RunStatus) {
        if self.stopping.is_some() {
            return;
        }
        self.resolve_pending();
        let mut grace_ends = Instant::now() + self.grace;
        let interrupt = json!({
            "type": "control_request",
            "request_id": Uuid::new_v4().to_string(),
            "request": {"subtype": "interrupt"},
        });
        if self.process.send(&interrupt).await.is_err() {
            // Nobody is listening, so there is nothing to wait for.
            grace_ends = Instant::now();
        }
        self.stopping = Some(Stopping { status, grace_ends });
    }

    /// The turn ended with a result message.
    async fn finish_turn(mut self, result: TurnResult) -> RunOutcome {
        if let Some(usage) = result.usage {
            self.emit(Event::Usage(usage));
        }
        match (result.outcome, self.stopping.as_ref().map(|stopping| stopping.status)) {
            (Ok(()), _) => self.finish(RunStatus::Completed, None).await,
            (Err(_), Some(status)) => self.finish(status, None).await,
            (Err(error), None) => self.finish(RunStatus::Failed, Some(error)).await,
        }
    }

    /// Something went wrong. After Taceta asked to stop, the stop is what the
    /// outcome reports.
    async fn finish_failed(self, error: String) -> RunOutcome {
        match self.stopping.as_ref().map(|stopping| stopping.status) {
            Some(status) => self.finish(status, None).await,
            None => self.finish(RunStatus::Failed, Some(error)).await,
        }
    }

    async fn finish(mut self, status: RunStatus, error: Option<String>) -> RunOutcome {
        self.resolve_pending();
        self.process.shutdown().await;
        RunOutcome { session_id: self.session_id, status, error }
    }

    async fn handle(&mut self, message: &Value) -> Result<Flow, String> {
        match required_str(message, "type", "a message")? {
            "system" => self.on_system(message)?,
            "stream_event" => self.on_stream_event(message)?,
            "assistant" => self.on_assistant(message)?,
            "user" => self.on_user(message)?,
            "control_request" => self.on_control_request(message).await?,
            "result" => return parse_result(message).map(Flow::Result),
            // rate_limit_event, control_response (the answer to our
            // interrupt), prompt_suggestion, and kinds added later.
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn on_system(&mut self, message: &Value) -> Result<(), String> {
        // The other subtypes report status and progress Taceta does not show.
        if message.get("subtype").and_then(Value::as_str) != Some("init") || self.session_announced {
            return Ok(());
        }
        let session_id = required_str(message, "session_id", "its init message")?.to_owned();
        self.session_id = Some(session_id.clone());
        self.session_announced = true;
        self.emit(Event::SessionReady { session_id });
        Ok(())
    }

    fn on_stream_event(&mut self, message: &Value) -> Result<(), String> {
        let event = message.get("event").ok_or("Claude Code sent a stream event without \"event\".")?;
        if required_str(event, "type", "a stream event")? != "content_block_delta" {
            return Ok(());
        }
        let delta = event.get("delta").ok_or("Claude Code sent a content delta without \"delta\".")?;
        let event = match required_str(delta, "type", "a content delta")? {
            "text_delta" => Event::MessageDelta(required_str(delta, "text", "a text delta")?.to_owned()),
            "thinking_delta" => {
                Event::ThinkingDelta(required_str(delta, "thinking", "a thinking delta")?.to_owned())
            }
            // input_json_delta, signature_delta, and kinds added later.
            _ => return Ok(()),
        };
        self.emit(event);
        Ok(())
    }

    fn on_assistant(&mut self, message: &Value) -> Result<(), String> {
        for block in content_blocks(message, "an assistant message")? {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let id = required_str(block, "id", "a tool call")?;
            let name = required_str(block, "name", "a tool call")?;
            let input = block.get("input").ok_or("Claude Code sent a tool call without \"input\".")?;
            if self.tools.contains_key(id) {
                // The same call again; its status must not go back to running.
                continue;
            }
            let (detail, diff) = describe_input(name, input);
            let item = ToolItem {
                id: id.to_owned(),
                title: tool_title(name, input),
                detail,
                diff,
                status: ToolStatus::Running,
            };
            self.tools.insert(id.to_owned(), item.clone());
            self.emit(Event::Tool(item));
        }
        Ok(())
    }

    fn on_user(&mut self, message: &Value) -> Result<(), String> {
        for block in content_blocks(message, "a user message")? {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = required_str(block, "tool_use_id", "a tool result")?;
            let failed = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let text = tool_result_text(block.get("content"))?;
            let item = self
                .tools
                .get_mut(id)
                .ok_or_else(|| format!("Claude Code returned a result for the unknown tool call {id}."))?;
            item.status = if failed { ToolStatus::Failed } else { ToolStatus::Completed };
            item.detail = bound(&text, TOOL_RESULT_LIMIT);
            let item = item.clone();
            self.emit(Event::Tool(item));
        }
        Ok(())
    }

    async fn on_control_request(&mut self, message: &Value) -> Result<(), String> {
        let request_id = required_str(message, "request_id", "a request")?.to_owned();
        let request = message.get("request").ok_or("Claude Code sent a request without \"request\".")?;
        match required_str(request, "subtype", "a request")? {
            "can_use_tool" => self.on_permission_request(request_id, request).await,
            _ => {
                self.process
                    .send(&json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "error",
                            "request_id": request_id,
                            "error": UNSUPPORTED_REQUEST,
                        },
                    }))
                    .await
            }
        }
    }

    async fn on_permission_request(&mut self, request_id: String, request: &Value) -> Result<(), String> {
        let tool = required_str(request, "tool_name", "a permission request")?;
        let input = request
            .get("input")
            .filter(|input| input.is_object())
            .ok_or("Claude Code sent a permission request without an input object.")?
            .clone();
        if self.stopping.is_some() || !self.controls_open {
            // Nobody can answer this one, so refuse it and end the turn.
            self.process.send(&permission_response(&request_id, None)).await?;
            self.begin_stop(RunStatus::Interrupted).await;
            return Ok(());
        }
        let title = request.get("display_name").and_then(Value::as_str).unwrap_or(tool).to_owned();
        let (detail, diff) = describe_input(tool, &input);
        let id = Uuid::new_v4();
        self.pending.insert(id, Pending { request_id, input });
        self.emit(Event::Approval(ApprovalRequest { id, title, detail: diff.unwrap_or(detail) }));
        Ok(())
    }

    /// The user's answer to a pending approval. An id that is not pending
    /// (already answered, withdrawn, or never asked) is ignored.
    async fn answer(&mut self, control: Control) -> Result<(), String> {
        let Control::Approval { id, approved } = control;
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        let response = permission_response(&pending.request_id, approved.then_some(&pending.input));
        let sent = self.process.send(&response).await;
        self.emit(Event::ApprovalResolved { id });
        sent
    }
}

/// `allowed_input` is `Some` to allow the action with that input.
fn permission_response(request_id: &str, allowed_input: Option<&Value>) -> Value {
    let response = match allowed_input {
        Some(input) => json!({"behavior": "allow", "updatedInput": input}),
        None => json!({"behavior": "deny", "message": DENIED_MESSAGE}),
    };
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response},
    })
}

fn required_str<'a>(value: &'a Value, key: &str, context: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Claude Code sent {context} without \"{key}\"."))
}

/// The content blocks of an assistant or user message. Plain-text content has
/// none that matter here.
fn content_blocks<'a>(message: &'a Value, context: &str) -> Result<&'a [Value], String> {
    match message.get("message").and_then(|inner| inner.get("content")) {
        Some(Value::Array(blocks)) => Ok(blocks),
        Some(Value::String(_)) => Ok(&[]),
        _ => Err(format!("Claude Code sent {context} without content.")),
    }
}

/// A tool result is a string or a list of text blocks. Other kinds of block
/// (images) have no text to show.
fn tool_result_text(content: Option<&Value>) -> Result<String, String> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(blocks)) => Ok(blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")),
        Some(_) => Err("Claude Code sent a tool result in a form Taceta does not understand.".to_owned()),
    }
}

fn parse_result(message: &Value) -> Result<TurnResult, String> {
    let subtype = required_str(message, "subtype", "a result")?;
    let reported_error = message.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let usage = parse_usage(message);
    if subtype == "success" && !reported_error {
        return Ok(TurnResult { outcome: Ok(()), usage: usage? });
    }
    // The failure itself matters more than a problem with its token counts.
    Ok(TurnResult { outcome: Err(failure_message(message, subtype)), usage: usage.unwrap_or(None) })
}

fn failure_message(message: &Value, subtype: &str) -> String {
    if let Some(text) = message.get("result").and_then(Value::as_str).filter(|text| !text.trim().is_empty()) {
        return text.to_owned();
    }
    if let Some(errors) = message.get("errors").and_then(Value::as_array) {
        let joined = errors.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ");
        if !joined.is_empty() {
            return joined;
        }
    }
    format!("Claude Code ended the turn with {subtype}.")
}

fn parse_usage(message: &Value) -> Result<Option<Usage>, String> {
    let Some(usage) = message.get("usage").filter(|usage| !usage.is_null()) else {
        return Ok(None);
    };
    let count = |key: &str, required: bool| match usage.get(key) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("Claude Code reported a token count \"{key}\" that is not a number.")),
        None if required => Err(format!("Claude Code reported usage without \"{key}\".")),
        None => Ok(0),
    };
    let input = count("input_tokens", true)?
        + count("cache_creation_input_tokens", false)?
        + count("cache_read_input_tokens", false)?;
    let output = count("output_tokens", true)?;
    // With helper models in play, the largest window is the main model's.
    let context_window = message
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|models| models.values().filter_map(|model| model.get("contextWindow")?.as_u64()).max());
    Ok(Some(Usage { input_tokens: input, output_tokens: output, context_window }))
}

/// What a person needs to judge a tool call: the detail text, and for file
/// edits a diff.
fn describe_input(tool: &str, input: &Value) -> (String, Option<String>) {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    match tool {
        "Bash" => {
            if let Some(command) = text("command") {
                return (command.to_owned(), None);
            }
        }
        "Edit" => {
            if let (Some(path), Some(old), Some(new)) = (text("file_path"), text("old_string"), text("new_string")) {
                return (path.to_owned(), Some(render_diff(path, old, new)));
            }
        }
        "Write" => {
            if let (Some(path), Some(content)) = (text("file_path"), text("content")) {
                return (path.to_owned(), Some(render_diff(path, "", content)));
            }
        }
        _ => {}
    }
    (compact_input(input), None)
}

/// What a tool call is about, for a list of actions: the tool and the
/// first line of its command, file, pattern or address.
fn tool_title(tool: &str, input: &Value) -> String {
    const SUBJECT_CHARS: usize = 80;
    let subject = ["command", "file_path", "path", "pattern", "url", "query", "description"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim);
    match subject {
        Some(subject) if subject.chars().count() > SUBJECT_CHARS => {
            format!("{tool}: {}…", subject.chars().take(SUBJECT_CHARS).collect::<String>())
        }
        Some(subject) => format!("{tool}: {subject}"),
        None => tool.to_owned(),
    }
}

fn render_diff(path: &str, removed: &str, added: &str) -> String {
    let mut diff = path.to_owned();
    for line in removed.lines() {
        diff.push_str("\n- ");
        diff.push_str(line);
    }
    for line in added.lines() {
        diff.push_str("\n+ ");
        diff.push_str(line);
    }
    bound(&diff, DIFF_LIMIT)
}

fn compact_input(input: &Value) -> String {
    let rendered = match input {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => format!("{key}: {text}"),
                other => format!("{key}: {other}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    bound(&rendered, INPUT_LIMIT)
}

/// Keeps the first `limit` characters and says how much was cut.
fn bound(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        None => text.to_owned(),
        Some((end, _)) => {
            let omitted = text[end..].chars().count();
            format!("{}\n[truncated: {omitted} more characters]", &text[..end])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines},
        task::JoinHandle,
    };

    const PROMPT: &str = "make c1.txt";

    fn request(approval: ApprovalPolicy) -> RunRequest {
        RunRequest {
            session_id: None,
            workspace: PathBuf::from("/work"),
            model: None,
            effort: None,
            prompt: PROMPT.to_owned(),
            approval,
            max_duration_secs: 30,
        }
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|arg| arg.into_string().unwrap()).collect()
    }

    /// The test plays the CLI: it reads what the driver writes and writes the
    /// lines the CLI would.
    struct Harness {
        to_driver: Option<DuplexStream>,
        from_driver: Lines<BufReader<DuplexStream>>,
        events: mpsc::UnboundedReceiver<Event>,
        seen: Vec<Event>,
        controls: mpsc::UnboundedSender<Control>,
        cancel: watch::Sender<bool>,
        run: JoinHandle<RunOutcome>,
    }

    fn start(request: RunRequest, duration: Duration, grace: Duration) -> Harness {
        let (driver_output, to_driver) = tokio::io::duplex(1 << 20);
        let (from_driver, driver_input) = tokio::io::duplex(1 << 20);
        let (events_tx, events) = mpsc::unbounded_channel();
        let (controls, controls_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let process = CliProcess::from_io(driver_output, driver_input);
        let run = tokio::spawn(drive(process, request, events_tx, controls_rx, cancel_rx, Limits { duration, grace }));
        Harness {
            to_driver: Some(to_driver),
            from_driver: BufReader::new(from_driver).lines(),
            events,
            seen: Vec::new(),
            controls,
            cancel,
            run,
        }
    }

    fn start_default(request: RunRequest) -> Harness {
        start(request, Duration::from_secs(30), Duration::from_secs(3))
    }

    impl Harness {
        async fn say(&mut self, message: Value) {
            let mut line = serde_json::to_vec(&message).unwrap();
            line.push(b'\n');
            self.to_driver.as_mut().expect("the CLI hung up").write_all(&line).await.unwrap();
        }

        /// The CLI's output ends.
        fn hang_up(&mut self) {
            self.to_driver = None;
        }

        async fn heard(&mut self) -> Value {
            let line = timeout(Duration::from_secs(5), self.from_driver.next_line())
                .await
                .expect("the driver wrote nothing")
                .unwrap()
                .expect("the driver closed its input");
            serde_json::from_str(&line).unwrap()
        }

        /// Reads events until one matches, keeping all of them.
        async fn event_where(&mut self, matches: impl Fn(&Event) -> bool) -> Event {
            loop {
                let event = timeout(Duration::from_secs(5), self.events.recv())
                    .await
                    .expect("no event arrived")
                    .expect("the event channel closed");
                self.seen.push(event.clone());
                if matches(&event) {
                    return event;
                }
            }
        }

        async fn approval(&mut self) -> ApprovalRequest {
            match self.event_where(|event| matches!(event, Event::Approval(_))).await {
                Event::Approval(request) => request,
                _ => unreachable!(),
            }
        }

        /// Waits for the run to end; returns its outcome, every event, and
        /// every line the driver wrote after the ones already read.
        async fn finish(mut self) -> (RunOutcome, Vec<Event>, Vec<Value>) {
            let outcome = timeout(Duration::from_secs(10), &mut self.run)
                .await
                .expect("the run did not end")
                .unwrap();
            while let Ok(event) = self.events.try_recv() {
                self.seen.push(event);
            }
            let mut rest = Vec::new();
            while let Some(line) = self.from_driver.next_line().await.unwrap() {
                rest.push(serde_json::from_str(&line).unwrap());
            }
            (outcome, self.seen, rest)
        }
    }

    fn prompt_line() -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": PROMPT}]}})
    }

    fn init(session: &str) -> Value {
        json!({"type": "system", "subtype": "init", "session_id": session, "model": "m", "permissionMode": "default"})
    }

    fn delta(kind: &str, key: &str, text: &str) -> Value {
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": kind, key: text}}})
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "on it"},
            {"type": "tool_use", "id": id, "name": name, "input": input},
        ]}})
    }

    fn permission(request_id: &str, tool: &str, tool_use_id: &str, input: Value) -> Value {
        json!({"type": "control_request", "request_id": request_id, "request": {
            "subtype": "can_use_tool", "tool_name": tool, "display_name": tool,
            "input": input, "description": "d", "tool_use_id": tool_use_id,
        }})
    }

    fn tool_result(id: &str, content: Value, is_error: bool) -> Value {
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error},
        ]}})
    }

    fn success() -> Value {
        json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "done", "session_id": "sess-1",
            "usage": {
                "input_tokens": 10, "output_tokens": 5,
                "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30,
            },
            "modelUsage": {"claude-x": {"contextWindow": 1_000_000}},
        })
    }

    fn interrupted_result() -> Value {
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "session_id": "sess-1"})
    }

    fn write_input() -> Value {
        json!({"file_path": "c1.txt", "content": "hello\nworld"})
    }

    fn allow(request_id: &str, input: Value) -> Value {
        json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": request_id,
            "response": {"behavior": "allow", "updatedInput": input},
        }})
    }

    fn deny(request_id: &str) -> Value {
        json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": request_id,
            "response": {"behavior": "deny", "message": "The user denied this action in Taceta."},
        }})
    }

    fn write_item(status: ToolStatus, detail: &str) -> ToolItem {
        ToolItem {
            id: "toolu_1".to_owned(),
            title: "Write: c1.txt".to_owned(),
            detail: detail.to_owned(),
            diff: Some("c1.txt\n+ hello\n+ world".to_owned()),
            status,
        }
    }

    #[tokio::test]
    async fn a_full_turn_streams_output_asks_for_approval_and_completes() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.say(json!({"type": "system", "subtype": "status", "status": "requesting"})).await;
        cli.say(json!({"type": "stream_event", "event": {"type": "message_start"}})).await;
        cli.say(delta("text_delta", "text", "Hel")).await;
        cli.say(delta("text_delta", "text", "lo")).await;
        cli.say(delta("thinking_delta", "thinking", "hmm")).await;
        cli.say(delta("signature_delta", "signature", "sig")).await;
        cli.say(json!({"type": "rate_limit_event"})).await;
        cli.say(tool_use("toolu_1", "Write", write_input())).await;
        cli.say(permission("req-1", "Write", "toolu_1", write_input())).await;
        let approval = cli.approval().await;
        cli.controls.send(Control::Approval { id: approval.id, approved: true }).unwrap();
        assert_eq!(cli.heard().await, allow("req-1", write_input()));
        cli.say(tool_result("toolu_1", json!("Created c1.txt"), false)).await;
        cli.say(success()).await;
        let (outcome, events, rest) = cli.finish().await;

        assert_eq!(outcome, RunOutcome { session_id: Some("sess-1".to_owned()), status: RunStatus::Completed, error: None });
        assert_eq!(
            events,
            vec![
                Event::SessionReady { session_id: "sess-1".to_owned() },
                Event::MessageDelta("Hel".to_owned()),
                Event::MessageDelta("lo".to_owned()),
                Event::ThinkingDelta("hmm".to_owned()),
                Event::Tool(write_item(ToolStatus::Running, "c1.txt")),
                Event::Approval(ApprovalRequest {
                    id: approval.id,
                    title: "Write".to_owned(),
                    detail: "c1.txt\n+ hello\n+ world".to_owned(),
                }),
                Event::ApprovalResolved { id: approval.id },
                Event::Tool(write_item(ToolStatus::Completed, "Created c1.txt")),
                Event::Usage(Usage { input_tokens: 60, output_tokens: 5, context_window: Some(1_000_000) }),
            ]
        );
        assert_eq!(rest, Vec::<Value>::new(), "nothing more is written after the allow response");
    }

    #[tokio::test]
    async fn a_denial_is_sent_to_the_cli_and_the_run_still_completes() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.say(tool_use("toolu_1", "Write", write_input())).await;
        cli.say(permission("req-1", "Write", "toolu_1", write_input())).await;
        let approval = cli.approval().await;
        cli.controls.send(Control::Approval { id: approval.id, approved: false }).unwrap();
        assert_eq!(cli.heard().await, deny("req-1"));
        cli.say(tool_result("toolu_1", json!("The user denied this action"), true)).await;
        cli.say(success()).await;
        let (outcome, events, _) = cli.finish().await;

        assert_eq!(outcome.status, RunStatus::Completed);
        assert!(events.contains(&Event::ApprovalResolved { id: approval.id }));
        assert!(events.contains(&Event::Tool(write_item(ToolStatus::Failed, "The user denied this action"))));
    }

    #[tokio::test]
    async fn cancelling_interrupts_the_cli_and_the_error_result_reports_interrupted() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.say(delta("text_delta", "text", "partial")).await;
        cli.event_where(|event| matches!(event, Event::MessageDelta(_))).await;
        cli.cancel.send(true).unwrap();
        let interrupt = cli.heard().await;
        assert_eq!(interrupt["type"], "control_request");
        assert_eq!(interrupt["request"], json!({"subtype": "interrupt"}));
        assert!(!interrupt["request_id"].as_str().unwrap().is_empty());
        cli.say(json!({"type": "control_response", "response": {"subtype": "success", "request_id": interrupt["request_id"]}})).await;
        cli.say(interrupted_result()).await;
        let (outcome, _, _) = cli.finish().await;

        assert_eq!(outcome, RunOutcome { session_id: Some("sess-1".to_owned()), status: RunStatus::Interrupted, error: None });
    }

    #[tokio::test]
    async fn a_cli_that_never_answers_the_interrupt_is_shut_down_after_the_grace_period() {
        let grace = Duration::from_millis(300);
        let mut cli = start(request(ApprovalPolicy::AskEveryTime), Duration::from_secs(30), grace);
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.event_where(|event| matches!(event, Event::SessionReady { .. })).await;
        let stopped = Instant::now();
        cli.cancel.send(true).unwrap();
        assert_eq!(cli.heard().await["request"], json!({"subtype": "interrupt"}));
        let (outcome, _, _) = cli.finish().await;

        let waited = stopped.elapsed();
        assert_eq!(outcome.status, RunStatus::Interrupted);
        assert!(waited >= grace - Duration::from_millis(50), "returned before the grace period: {waited:?}");
        assert!(waited < grace + Duration::from_secs(2), "did not return after the grace period: {waited:?}");
    }

    #[tokio::test]
    async fn the_time_limit_includes_approval_waiting_and_reports_limit_reached() {
        let mut cli = start(request(ApprovalPolicy::AskEveryTime), Duration::from_millis(200), Duration::from_secs(3));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.say(tool_use("toolu_1", "Write", write_input())).await;
        cli.say(permission("req-1", "Write", "toolu_1", write_input())).await;
        let approval = cli.approval().await;
        // Nobody answers the approval; the limit arrives while it is pending.
        assert_eq!(cli.heard().await["request"], json!({"subtype": "interrupt"}));
        cli.say(interrupted_result()).await;
        let (outcome, events, _) = cli.finish().await;

        assert_eq!(outcome.status, RunStatus::LimitReached);
        assert!(events.contains(&Event::ApprovalResolved { id: approval.id }), "the pending approval was left open");
    }

    #[tokio::test]
    async fn output_that_ends_without_a_result_is_a_failure() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.say(delta("text_delta", "text", "half an answer")).await;
        cli.hang_up();
        let (outcome, _, _) = cli.finish().await;

        assert_eq!(
            outcome,
            RunOutcome::failed(Some("sess-1".to_owned()), "Claude Code ended before finishing the turn.")
        );
    }

    #[tokio::test]
    async fn an_approval_for_an_unknown_id_is_ignored_and_the_real_one_still_works() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(init("sess-1")).await;
        cli.say(permission("req-1", "Write", "toolu_1", write_input())).await;
        let approval = cli.approval().await;
        cli.controls.send(Control::Approval { id: Uuid::new_v4(), approved: false }).unwrap();
        cli.controls.send(Control::Approval { id: approval.id, approved: true }).unwrap();
        assert_eq!(cli.heard().await, allow("req-1", write_input()));
        cli.say(success()).await;
        let (outcome, events, rest) = cli.finish().await;

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(rest, Vec::<Value>::new());
        assert_eq!(events.iter().filter(|event| matches!(event, Event::ApprovalResolved { .. })).count(), 1);
    }

    #[tokio::test]
    async fn a_request_of_another_kind_gets_an_error_response() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(json!({"type": "control_request", "request_id": "req-9", "request": {"subtype": "mcp_message"}})).await;
        assert_eq!(
            cli.heard().await,
            json!({"type": "control_response", "response": {
                "subtype": "error", "request_id": "req-9", "error": "Taceta does not support this request.",
            }})
        );
        cli.say(success()).await;
        let (outcome, _, _) = cli.finish().await;
        assert_eq!(outcome.status, RunStatus::Completed);
    }

    #[tokio::test]
    async fn an_error_result_fails_the_run_with_the_cli_message() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(json!({"type": "result", "subtype": "error_max_turns", "is_error": true, "errors": ["Reached max turns"]})).await;
        let (outcome, _, _) = cli.finish().await;
        assert_eq!(outcome, RunOutcome::failed(None, "Reached max turns"));
    }

    #[tokio::test]
    async fn a_malformed_delta_fails_the_run_instead_of_being_skipped() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta"}}})).await;
        let (outcome, _, _) = cli.finish().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(outcome.error.as_deref(), Some("Claude Code sent a text delta without \"text\"."));
    }

    #[tokio::test]
    async fn dropping_the_event_receiver_stops_the_cli() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        drop(std::mem::replace(&mut cli.events, mpsc::unbounded_channel().1));
        assert_eq!(cli.heard().await["request"], json!({"subtype": "interrupt"}));
        cli.say(interrupted_result()).await;
        let (outcome, _, _) = cli.finish().await;
        assert_eq!(outcome.status, RunStatus::Interrupted);
    }

    #[tokio::test]
    async fn closing_the_controls_while_an_approval_is_pending_stops_the_cli() {
        let mut cli = start_default(request(ApprovalPolicy::AskEveryTime));
        assert_eq!(cli.heard().await, prompt_line());
        cli.say(permission("req-1", "Write", "toolu_1", write_input())).await;
        let approval = cli.approval().await;
        let controls = std::mem::replace(&mut cli.controls, mpsc::unbounded_channel().0);
        drop(controls);
        assert_eq!(cli.heard().await["request"], json!({"subtype": "interrupt"}));
        cli.say(interrupted_result()).await;
        let (outcome, events, _) = cli.finish().await;
        assert_eq!(outcome.status, RunStatus::Interrupted);
        assert!(events.contains(&Event::ApprovalResolved { id: approval.id }));
    }

    #[test]
    fn arguments_ask_every_time_sets_manual_permission_mode_and_follow_cli_leaves_it_out() {
        let base = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ];
        let ask = strings(build_args(&request(ApprovalPolicy::AskEveryTime)));
        assert_eq!(ask, [&base[..], &["--permission-mode", "manual"]].concat());
        let follow = strings(build_args(&request(ApprovalPolicy::FollowCli)));
        assert_eq!(follow, base);
        // A conversation without tools starts with no built-in tool and no MCP server.
        let none = strings(build_args(&request(ApprovalPolicy::NoTools)));
        assert_eq!(&none[..base.len()], base);
        assert_eq!(
            &none[base.len()..],
            ["--permission-mode", "manual", "--tools", "", "--strict-mcp-config", "--append-system-prompt", NO_TOOLS_NOTE]
        );
    }

    #[test]
    fn arguments_carry_model_effort_and_resume_only_when_given() {
        let mut full = request(ApprovalPolicy::FollowCli);
        full.model = Some("opus".to_owned());
        full.effort = Some("high".to_owned());
        full.session_id = Some("sess-1".to_owned());
        let args = strings(build_args(&full));
        assert_eq!(&args[9..], ["--model", "opus", "--effort", "high", "--resume", "sess-1"]);

        let mut model_only = request(ApprovalPolicy::AskEveryTime);
        model_only.model = Some("haiku".to_owned());
        let args = strings(build_args(&model_only));
        assert_eq!(&args[9..], ["--permission-mode", "manual", "--model", "haiku"]);
        assert!(!args.iter().any(|arg| arg == "--effort" || arg == "--resume"));
        assert!(!args.iter().any(|arg| arg == PROMPT), "the prompt must not be an argument");
    }

    #[test]
    fn edits_and_commands_are_rendered_for_a_person_to_judge() {
        let edit = json!({"file_path": "a.rs", "old_string": "one\ntwo", "new_string": "three"});
        assert_eq!(describe_input("Edit", &edit), ("a.rs".to_owned(), Some("a.rs\n- one\n- two\n+ three".to_owned())));
        assert_eq!(describe_input("Bash", &json!({"command": "ls -la"})), ("ls -la".to_owned(), None));
        assert_eq!(
            describe_input("Grep", &json!({"pattern": "x", "limit": 3})),
            ("limit: 3\npattern: x".to_owned(), None)
        );
    }

    #[test]
    fn long_tool_output_is_cut_with_an_explicit_marker_and_block_lists_are_joined() {
        let long = "あ".repeat(TOOL_RESULT_LIMIT + 7);
        let cut = bound(&long, TOOL_RESULT_LIMIT);
        assert!(cut.starts_with(&"あ".repeat(TOOL_RESULT_LIMIT)));
        assert!(cut.ends_with("[truncated: 7 more characters]"));
        assert_eq!(bound("short", TOOL_RESULT_LIMIT), "short");
        let blocks = json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}]);
        assert_eq!(tool_result_text(Some(&blocks)).unwrap(), "a\nb");
    }

    #[tokio::test]
    async fn the_known_models_offer_every_effort() {
        let models = ClaudeCode.models().await.unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["fable", "opus", "sonnet", "haiku"]);
        assert!(models.iter().all(|model| model.efforts == EFFORTS && model.default_effort.is_none()));
    }
}
