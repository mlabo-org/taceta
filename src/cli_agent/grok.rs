//! Driver for the Grok CLI in agent mode (`grok agent stdio`).
//!
//! The CLI speaks the Agent Client Protocol: JSON-RPC 2.0, one JSON object per
//! line on stdio. This file turns that protocol into the types in
//! [`super`]. The user signs in with the CLI itself; the driver uses the
//! user's normal `~/.grok` settings and never handles credentials.

use super::{
    ApprovalPolicy, ApprovalRequest, CliAgent, CliFuture, Control, Event, ModelOption, Readiness,
    RunOutcome, RunRequest, RunStatus, ToolItem, ToolStatus, Usage,
    process::{CliProcess, find_executable, usual_install_dirs},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use uuid::Uuid;

const EXECUTABLE: &str = "grok";

const NOT_FOUND: &str = "The Grok CLI was not found. Install it and sign in with it yourself; Taceta never signs in for you.";

/// The CLI's read-only tools. The approval hook lets these through without
/// asking; everything else is sent to Taceta.
const READ_ONLY_TOOLS: [&str; 6] = [
    "read_file",
    "list_dir",
    "grep",
    "web_search",
    "todo_write",
    "get_command_or_subagent_output",
];

#[derive(Clone, Copy, Debug)]
struct Timing {
    /// How long one startup step may wait for the CLI's answer.
    startup: Duration,
    /// How long a cancelled turn may take to answer before the CLI is ended.
    cancel_grace: Duration,
    /// How long to wait after `initialize` for the CLI to announce its
    /// current model list.
    model_refresh: Duration,
}

impl Timing {
    const DEFAULT: Self = Self {
        startup: Duration::from_secs(60),
        cancel_grace: Duration::from_secs(3),
        model_refresh: Duration::from_secs(10),
    };
}

pub struct GrokCli {
    /// Where the approval hook plugin is written. Used only for
    /// [`ApprovalPolicy::AskEveryTime`].
    approval_plugin_dir: PathBuf,
    /// The CLI that answered the last model listing, kept until its
    /// refreshed list has been waited for.
    listing: std::sync::Arc<tokio::sync::Mutex<Option<CliProcess>>>,
}

impl GrokCli {
    pub fn new(approval_plugin_dir: PathBuf) -> Self {
        Self {
            approval_plugin_dir,
            listing: Default::default(),
        }
    }
}

impl CliAgent for GrokCli {
    fn name(&self) -> &'static str {
        "Grok"
    }

    fn readiness(&self) -> CliFuture<Result<Readiness, String>> {
        Box::pin(async {
            let executable = locate()?;
            let version = read_version(&executable).await?;
            Ok(Readiness {
                executable,
                version,
                signed_in: None,
            })
        })
    }

    fn models(&self) -> CliFuture<Result<Vec<ModelOption>, String>> {
        let plugin_dir = self.approval_plugin_dir.clone();
        let listing = self.listing.clone();
        Box::pin(async move {
            let executable = locate()?;
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/".into());
            let args = agent_args(ApprovalPolicy::FollowCli, &plugin_dir);
            let mut process = CliProcess::spawn(&executable, &args, &home)?;
            match stored_models(&mut process, Timing::DEFAULT).await {
                // The stored list is offered at once; the CLI stays up for
                // `refreshed_models` to wait for the current one.
                Ok(Some(models)) => {
                    if let Some(previous) = listing.lock().await.replace(process) {
                        previous.shutdown().await;
                    }
                    Ok(models)
                }
                Ok(None) => {
                    let announced = announced_models(&mut process, Timing::DEFAULT).await;
                    process.shutdown().await;
                    announced.ok_or_else(|| "Grok's initialize answer lists no models.".to_owned())
                }
                Err(error) => {
                    process.shutdown().await;
                    Err(error)
                }
            }
        })
    }

    fn refreshed_models(&self) -> CliFuture<Option<Vec<ModelOption>>> {
        let listing = self.listing.clone();
        Box::pin(async move {
            let mut process = listing.lock().await.take()?;
            let announced = announced_models(&mut process, Timing::DEFAULT).await;
            process.shutdown().await;
            announced
        })
    }

    fn run(
        &self,
        request: RunRequest,
        events: mpsc::UnboundedSender<Event>,
        controls: mpsc::UnboundedReceiver<Control>,
        cancel: watch::Receiver<bool>,
    ) -> CliFuture<RunOutcome> {
        let plugin_dir = self.approval_plugin_dir.clone();
        Box::pin(async move {
            let session_id = request.session_id.clone();
            let executable = match locate() {
                Ok(path) => path,
                Err(error) => return RunOutcome::failed(session_id, error),
            };
            let written = match request.approval {
                ApprovalPolicy::AskEveryTime => write_approval_plugin(&plugin_dir),
                ApprovalPolicy::NoTools => write_refusal_plugin(&refusal_plugin_dir(&plugin_dir)),
                ApprovalPolicy::FollowCli => Ok(()),
            };
            if let Err(error) = written {
                return RunOutcome::failed(session_id, error);
            }
            let args = agent_args(request.approval, &plugin_dir);
            let process = match CliProcess::spawn(&executable, &args, &request.workspace) {
                Ok(process) => process,
                Err(error) => return RunOutcome::failed(session_id, error),
            };
            run_session(process, request, events, controls, cancel, Timing::DEFAULT).await
        })
    }
}

fn locate() -> Result<PathBuf, String> {
    find_executable(EXECUTABLE, &usual_install_dirs()).ok_or_else(|| NOT_FOUND.to_owned())
}

/// The first line of `grok --version`. This starts no session.
async fn read_version(executable: &Path) -> Result<Option<String>, String> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "`grok --version` did not finish within 10 seconds.".to_owned())?
        .map_err(|error| format!("Could not run {}: {error}", executable.display()))?;
    if !output.status.success() {
        return Err(format!("`grok --version` failed ({}).", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned))
}

/// Arguments for starting the CLI as an ACP agent. Taceta's hook is loaded
/// as a plugin when every action must be asked about, or refused.
fn agent_args(approval: ApprovalPolicy, plugin_dir: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["agent".into()];
    match approval {
        ApprovalPolicy::AskEveryTime => {
            args.push("--plugin-dir".into());
            args.push(plugin_dir.as_os_str().to_owned());
        }
        ApprovalPolicy::NoTools => {
            args.push("--plugin-dir".into());
            args.push(refusal_plugin_dir(plugin_dir).into_os_string());
        }
        ApprovalPolicy::FollowCli => {}
    }
    args.push("stdio".into());
    args
}

// ---------------------------------------------------------------------------
// Approval hook plugin
// ---------------------------------------------------------------------------

const ASK_SCRIPT_NAME: &str = "ask.sh";

fn ask_script() -> String {
    let read_only = READ_ONLY_TOOLS.join("|");
    format!(
        r#"#!/bin/sh
# Written by Taceta. Asks before every tool except the CLI's read-only ones.
input=$(cat)
name=$(printf '%s' "$input" | sed -n 's/.*"toolName"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
case "$name" in
  {read_only})
    printf '%s\n' '{{"decision":"defer"}}'
    ;;
  *)
    printf '%s\n' '{{"decision":"ask","reason":"Taceta asks before every action"}}'
    ;;
esac
"#
    )
}

/// The CLI has no switch that starts it without tools, so a conversation
/// without tools refuses each call through the same documented hook.
const REFUSAL_SCRIPT: &str = r#"#!/bin/sh
# Written by Taceta. Refuses every tool: this conversation has none.
cat > /dev/null
printf '%s\n' '{"decision":"deny","reason":"This conversation has no tools. Answer from the conversation itself."}'
"#;

/// The refusing plugin lives beside the asking one.
fn refusal_plugin_dir(approval_plugin_dir: &Path) -> PathBuf {
    let mut name = approval_plugin_dir.file_name().unwrap_or_default().to_os_string();
    name.push("NoTools");
    approval_plugin_dir.with_file_name(name)
}

fn write_approval_plugin(dir: &Path) -> Result<(), String> {
    write_hook_plugin(dir, &ask_script())
}

fn write_refusal_plugin(dir: &Path) -> Result<(), String> {
    write_hook_plugin(dir, REFUSAL_SCRIPT)
}

/// Writes `<dir>/hooks/hooks.json` and `<dir>/hooks/ask.sh`, replacing any
/// stale content. The directories and the script are readable by the user
/// only.
fn write_hook_plugin(dir: &Path, script: &str) -> Result<(), String> {
    if !dir.is_absolute() {
        return Err(format!(
            "The approval plugin directory must be absolute: {}",
            dir.display()
        ));
    }
    let hooks_dir = dir.join("hooks");
    let script_path = hooks_dir.join(ASK_SCRIPT_NAME);
    let script_text = script_path
        .to_str()
        .ok_or("The approval plugin path is not valid UTF-8.")?;
    // The CLI runs the command through a shell, so the path is quoted: the
    // usual location is under "Application Support". An unquoted path fails
    // to start, and a hook that fails does not stop the tool call.
    let command = format!("'{}'", script_text.replace('\'', r"'\''"));
    let hooks = json!({
        "hooks": {
            "PreToolUse": [{
                "hooks": [{"type": "command", "command": command, "timeout": 10}]
            }]
        }
    });
    let hooks_text = serde_json::to_string_pretty(&hooks).map_err(|error| error.to_string())?;
    create_private_dir(dir)?;
    create_private_dir(&hooks_dir)?;
    replace_file(&hooks_dir.join("hooks.json"), hooks_text.as_bytes(), 0o600)?;
    replace_file(&script_path, script.as_bytes(), 0o700)
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let describe = |error: std::io::Error| {
        format!(
            "Could not prepare the approval plugin at {}: {error}",
            path.display()
        )
    };
    std::fs::create_dir_all(path).map_err(describe)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(describe)
}

/// Writes next to the target and renames, so a CLI that starts at the same
/// time never reads a half-written file.
fn replace_file(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let describe = |error: std::io::Error| {
        format!(
            "Could not write the approval plugin file {}: {error}",
            path.display()
        )
    };
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temporary, content).map_err(describe)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))
        .map_err(describe)?;
    std::fs::rename(&temporary, path).map_err(describe)
}

// ---------------------------------------------------------------------------
// JSON-RPC plumbing
// ---------------------------------------------------------------------------

fn request_message(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification_message(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn result_message(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn unsupported_message(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32601, "message": "Taceta does not support this request."}
    })
}

fn permission_outcome(option_id: Option<&str>) -> Value {
    match option_id {
        Some(option_id) => json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, String>,
    },
}

fn classify(message: Value) -> Result<Incoming, String> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = message.get("id").filter(|id| !id.is_null()).cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => Ok(Incoming::Request { id, method, params }),
        (Some(method), None) => Ok(Incoming::Notification { method, params }),
        (None, Some(id)) => {
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string());
                Ok(Incoming::Response {
                    id,
                    result: Err(text),
                })
            } else if let Some(result) = message.get("result") {
                Ok(Incoming::Response {
                    id,
                    result: Ok(result.clone()),
                })
            } else {
                Err("Grok sent a response with neither a result nor an error.".to_owned())
            }
        }
        (None, None) => Err(
            "Grok sent a message that is neither a request, a response, nor a notification."
                .to_owned(),
        ),
    }
}

/// Why a run stops before the CLI finishes the turn on its own.
#[derive(Clone, Debug)]
enum Stop {
    Interrupted,
    LimitReached,
    Failed(String),
}

/// What can interrupt a wait: the caller's cancel flag and the time limit.
struct Watch {
    cancel: Option<watch::Receiver<bool>>,
    deadline: Option<Instant>,
}

impl Watch {
    fn none() -> Self {
        Self {
            cancel: None,
            deadline: None,
        }
    }
}

/// Resolves when the flag is set. Never resolves when there is no flag, or
/// when its sender is gone without having set it.
async fn cancelled(cancel: &mut Option<watch::Receiver<bool>>) {
    if let Some(receiver) = cancel {
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
    std::future::pending::<()>().await;
}

async fn sleep_until_option(instant: Option<Instant>) {
    match instant {
        Some(instant) => sleep_until(instant).await,
        None => std::future::pending::<()>().await,
    }
}

/// Sends one request and waits for its response. Everything else the CLI
/// writes meanwhile is ignored: before the prompt is sent no content may be
/// emitted, and the CLI may replay a loaded session.
async fn call(
    process: &mut CliProcess,
    id: u64,
    method: &str,
    params: Value,
    step: &str,
    timeout: Duration,
    watch: &mut Watch,
) -> Result<Value, Stop> {
    process
        .send(&request_message(id, method, params))
        .await
        .map_err(Stop::Failed)?;
    let step_deadline = Instant::now() + timeout;
    loop {
        tokio::select! {
            biased;
            _ = cancelled(&mut watch.cancel) => return Err(Stop::Interrupted),
            _ = sleep_until_option(watch.deadline) => return Err(Stop::LimitReached),
            _ = sleep_until(step_deadline) => {
                return Err(Stop::Failed(format!(
                    "Grok did not answer {step} within {timeout:?}."
                )));
            }
            message = process.next() => match message {
                None => return Err(Stop::Failed(format!("Grok ended during {step}."))),
                Some(Err(error)) => return Err(Stop::Failed(error)),
                Some(Ok(message)) => match classify(message).map_err(Stop::Failed)? {
                    Incoming::Response { id: response_id, result } => {
                        if response_id.as_u64() == Some(id) {
                            return result.map_err(|error| {
                                Stop::Failed(format!("Grok rejected {step}: {error}"))
                            });
                        }
                    }
                    Incoming::Request { id, .. } => {
                        process.send(&unsupported_message(&id)).await.map_err(Stop::Failed)?;
                    }
                    Incoming::Notification { .. } => {}
                },
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false
        },
        "clientInfo": {"name": "taceta", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// The models listed in a `modelState` / `models` object.
fn parse_model_options(state: &Value) -> Result<Vec<ModelOption>, String> {
    let models = state
        .get("availableModels")
        .and_then(Value::as_array)
        .ok_or("Grok's answer has no list of available models.")?;
    models.iter().map(parse_model_option).collect()
}

fn parse_model_option(model: &Value) -> Result<ModelOption, String> {
    let id = model
        .get("modelId")
        .and_then(Value::as_str)
        .ok_or("A model in Grok's list has no modelId.")?;
    let label = model.get("name").and_then(Value::as_str).unwrap_or(id);
    let meta = model.get("_meta");
    let context_window = meta
        .and_then(|meta| meta.get("totalContextTokens"))
        .and_then(Value::as_u64);
    let listed = meta
        .and_then(|meta| meta.get("reasoningEfforts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut efforts = Vec::new();
    let mut default_effort = None;
    for effort in listed {
        let effort_id = effort
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("A reasoning effort of model {id} has no id."))?;
        if effort.get("default").and_then(Value::as_bool) == Some(true) {
            default_effort = Some(effort_id.to_owned());
        }
        efforts.push(effort_id.to_owned());
    }
    // The CLI lists the strongest effort first; the interface wants weakest first.
    efforts.reverse();
    Ok(ModelOption {
        id: id.to_owned(),
        label: label.to_owned(),
        context_window,
        efforts,
        default_effort,
    })
}

/// Starts the protocol and returns the model list the CLI had stored, which
/// `initialize` carries. `None` when it carries none.
async fn stored_models(
    process: &mut CliProcess,
    timing: Timing,
) -> Result<Option<Vec<ModelOption>>, String> {
    let result = call(
        process,
        1,
        "initialize",
        initialize_params(),
        "initialize",
        timing.startup,
        &mut Watch::none(),
    )
    .await
    .map_err(|stop| match stop {
        Stop::Failed(message) => message,
        Stop::Interrupted | Stop::LimitReached => "Listing Grok's models was stopped.".to_owned(),
    })?;
    result.pointer("/_meta/modelState").map(parse_model_options).transpose()
}

/// After `initialize` the CLI fetches the current model list and announces
/// it, sometimes more than once. The stored list can lack the newest models,
/// so the last list announced within the wait replaces it.
async fn announced_models(process: &mut CliProcess, timing: Timing) -> Option<Vec<ModelOption>> {
    let mut announced = None;
    let _ = tokio::time::timeout(timing.model_refresh, async {
        while let Some(Ok(message)) = process.next().await {
            if message.get("method").and_then(Value::as_str) == Some("_x.ai/models/update") {
                let listed = message.get("params").filter(|state| state.get("availableModels").is_some());
                if let Some(models) = listed.and_then(|state| parse_model_options(state).ok()) {
                    announced = Some(models);
                }
            }
        }
    })
    .await;
    announced
}

// ---------------------------------------------------------------------------
// A run
// ---------------------------------------------------------------------------

async fn run_session(
    mut process: CliProcess,
    request: RunRequest,
    events: mpsc::UnboundedSender<Event>,
    controls: mpsc::UnboundedReceiver<Control>,
    cancel: watch::Receiver<bool>,
    timing: Timing,
) -> RunOutcome {
    let outcome = drive(&mut process, &request, events, controls, cancel, timing).await;
    process.close_input();
    process.shutdown().await;
    outcome
}

struct Started {
    session_id: String,
    models: Vec<ModelOption>,
    current_model: Option<String>,
}

fn stop_outcome(session_id: Option<String>, stop: Stop) -> RunOutcome {
    match stop {
        Stop::Interrupted => RunOutcome {
            session_id,
            status: RunStatus::Interrupted,
            error: None,
        },
        Stop::LimitReached => RunOutcome {
            session_id,
            status: RunStatus::LimitReached,
            error: None,
        },
        Stop::Failed(message) => RunOutcome::failed(session_id, message),
    }
}

async fn drive(
    process: &mut CliProcess,
    request: &RunRequest,
    events: mpsc::UnboundedSender<Event>,
    controls: mpsc::UnboundedReceiver<Control>,
    cancel: watch::Receiver<bool>,
    timing: Timing,
) -> RunOutcome {
    let mut watch = Watch {
        cancel: Some(cancel),
        deadline: Some(Instant::now() + Duration::from_secs(request.max_duration_secs)),
    };
    let started = match start_session(process, request, &events, timing, &mut watch).await {
        Ok(started) => started,
        Err((session_id, stop)) => return stop_outcome(session_id, stop),
    };
    let chosen_model = request.model.clone().or(started.current_model);
    let mut turn = Turn {
        events,
        controls,
        controls_open: true,
        watch,
        session_id: started.session_id,
        models: started.models,
        chosen_model,
        grace: timing.cancel_grace,
        grace_until: None,
        tools: HashMap::new(),
        pending: Vec::new(),
        stop: None,
        refused: false,
    };
    let prompt_id = 100;
    let prompt = json!({
        "sessionId": turn.session_id,
        "prompt": [{"type": "text", "text": request.prompt}]
    });
    if let Err(error) = process
        .send(&request_message(prompt_id, "session/prompt", prompt))
        .await
    {
        return RunOutcome::failed(Some(turn.session_id), error);
    }
    turn.pump(process, prompt_id).await
}

/// initialize → session/new or session/load → SessionReady → model → effort.
async fn start_session(
    process: &mut CliProcess,
    request: &RunRequest,
    events: &mpsc::UnboundedSender<Event>,
    timing: Timing,
    watch: &mut Watch,
) -> Result<Started, (Option<String>, Stop)> {
    let known = request.session_id.clone();
    let fail = |known: &Option<String>, stop: Stop| (known.clone(), stop);
    let workspace = request
        .workspace
        .to_str()
        .filter(|_| request.workspace.is_absolute())
        .ok_or_else(|| {
            fail(
                &known,
                Stop::Failed(format!(
                    "The workspace must be an absolute UTF-8 path: {}",
                    request.workspace.display()
                )),
            )
        })?;

    let initialized = call(
        process,
        1,
        "initialize",
        initialize_params(),
        "initialize",
        timing.startup,
        watch,
    )
    .await
    .map_err(|stop| fail(&known, stop))?;

    let (session_id, session_result) = match &request.session_id {
        Some(session_id) => {
            let can_load = initialized
                .pointer("/agentCapabilities/loadSession")
                .and_then(Value::as_bool)
                == Some(true);
            if !can_load {
                return Err(fail(
                    &known,
                    Stop::Failed("This Grok CLI cannot continue an earlier session.".to_owned()),
                ));
            }
            let params = json!({"sessionId": session_id, "cwd": workspace, "mcpServers": []});
            let result = call(
                process,
                2,
                "session/load",
                params,
                "session/load",
                timing.startup,
                watch,
            )
            .await
            .map_err(|stop| fail(&known, stop))?;
            (session_id.clone(), result)
        }
        None => {
            let params = json!({"cwd": workspace, "mcpServers": []});
            let result = call(
                process,
                2,
                "session/new",
                params,
                "session/new",
                timing.startup,
                watch,
            )
            .await
            .map_err(|stop| fail(&known, stop))?;
            let session_id = result
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    fail(
                        &known,
                        Stop::Failed("Grok's session/new answer has no sessionId.".into()),
                    )
                })?
                .to_owned();
            (session_id, result)
        }
    };
    let known = Some(session_id.clone());

    // The model list is only used to report the context window, so a missing
    // or odd list means "unknown", not a failed run.
    let model_state = session_result
        .get("models")
        .filter(|models| models.get("availableModels").is_some())
        .or_else(|| initialized.pointer("/_meta/modelState"));
    let models = model_state
        .and_then(|state| parse_model_options(state).ok())
        .unwrap_or_default();
    let current_model = model_state
        .and_then(|state| state.get("currentModelId"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    if events
        .send(Event::SessionReady {
            session_id: session_id.clone(),
        })
        .is_err()
    {
        return Err(fail(&known, Stop::Interrupted));
    }

    let mut next_id = 3;
    let options = [
        ("model", "set model", &request.model),
        ("reasoning_effort", "set reasoning effort", &request.effort),
    ];
    for (config_id, step, value) in options {
        let Some(value) = value else { continue };
        // The chosen value goes as a plain string, as the protocol describes.
        let params = json!({"sessionId": session_id, "configId": config_id, "value": value});
        call(process, next_id, "session/set_config_option", params, step, timing.startup, watch)
            .await
            .map_err(|stop| fail(&known, stop))?;
        next_id += 1;
    }
    Ok(Started {
        session_id,
        models,
        current_model,
    })
}

struct PermissionOption {
    id: String,
    kind: String,
}

struct PendingApproval {
    id: Uuid,
    request_id: Value,
    options: Vec<PermissionOption>,
}

struct Turn {
    events: mpsc::UnboundedSender<Event>,
    controls: mpsc::UnboundedReceiver<Control>,
    controls_open: bool,
    watch: Watch,
    session_id: String,
    models: Vec<ModelOption>,
    chosen_model: Option<String>,
    grace: Duration,
    grace_until: Option<Instant>,
    tools: HashMap<String, ToolItem>,
    pending: Vec<PendingApproval>,
    /// Set when this run decided to end the turn early.
    stop: Option<Stop>,
    /// The user refused an action in this turn. The CLI may then end the
    /// turn as cancelled, which is the turn's ordinary end, not a failure.
    refused: bool,
}

/// How the wait for the prompt's response ended.
enum Ending {
    Response(Result<Value, String>),
    OutputEnded,
    ReadFailed(String),
    GraceExpired,
}

impl Turn {
    fn emit(&self, event: Event) {
        // A closed receiver is noticed by the pump, which stops the run.
        let _ = self.events.send(event);
    }

    async fn pump(&mut self, process: &mut CliProcess, prompt_id: u64) -> RunOutcome {
        loop {
            tokio::select! {
                biased;
                _ = cancelled(&mut self.watch.cancel), if self.stop.is_none() => {
                    self.begin_stop(process, Stop::Interrupted).await;
                }
                _ = sleep_until_option(self.watch.deadline), if self.stop.is_none() => {
                    self.begin_stop(process, Stop::LimitReached).await;
                }
                _ = self.events.closed(), if self.stop.is_none() => {
                    self.begin_stop(process, Stop::Interrupted).await;
                }
                _ = sleep_until_option(self.grace_until), if self.grace_until.is_some() => {
                    return self.conclude(Ending::GraceExpired);
                }
                control = self.controls.recv(), if self.controls_open => match control {
                    Some(Control::Approval { id, approved }) => {
                        if let Err(error) = self.answer_approval(process, id, approved).await {
                            self.begin_stop(process, Stop::Failed(error)).await;
                        }
                    }
                    None => {
                        self.controls_open = false;
                        if !self.pending.is_empty() {
                            self.begin_stop(process, Stop::Interrupted).await;
                        }
                    }
                },
                message = process.next() => match message {
                    None => return self.conclude(Ending::OutputEnded),
                    Some(Err(error)) => return self.conclude(Ending::ReadFailed(error)),
                    Some(Ok(message)) => match self.on_message(process, prompt_id, message).await {
                        Ok(Some(response)) => return self.conclude(Ending::Response(response)),
                        Ok(None) => {}
                        Err(error) => self.begin_stop(process, Stop::Failed(error)).await,
                    },
                },
            }
        }
    }

    /// Ends the turn early: tells the CLI to cancel and answers every
    /// pending permission request with "cancelled". The first reason wins.
    async fn begin_stop(&mut self, process: &mut CliProcess, stop: Stop) {
        if self.stop.is_some() {
            return;
        }
        self.stop = Some(stop);
        self.watch.deadline = None;
        self.grace_until = Some(Instant::now() + self.grace);
        // If the CLI is already gone, the end of its output reports that.
        let _ = process
            .send(&notification_message(
                "session/cancel",
                json!({"sessionId": self.session_id}),
            ))
            .await;
        for approval in std::mem::take(&mut self.pending) {
            let _ = process
                .send(&result_message(
                    &approval.request_id,
                    permission_outcome(None),
                ))
                .await;
            self.emit(Event::ApprovalResolved { id: approval.id });
        }
    }

    fn conclude(&mut self, ending: Ending) -> RunOutcome {
        for approval in std::mem::take(&mut self.pending) {
            self.emit(Event::ApprovalResolved { id: approval.id });
        }
        let session_id = Some(self.session_id.clone());
        let mut stop_reason = None;
        let mut failure = None;
        match ending {
            Ending::Response(Ok(result)) => {
                self.emit_usage(&result);
                stop_reason = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if stop_reason.is_none() {
                    failure = Some("Grok's answer to the prompt has no stopReason.".to_owned());
                }
            }
            Ending::Response(Err(message)) => failure = Some(message),
            Ending::OutputEnded => {
                failure = Some("Grok ended before finishing the turn.".to_owned());
            }
            Ending::ReadFailed(error) => failure = Some(error),
            Ending::GraceExpired => {}
        }
        if let Some(stop) = self.stop.take() {
            return stop_outcome(session_id, stop);
        }
        if let Some(message) = failure {
            return RunOutcome::failed(session_id, message);
        }
        match stop_reason.as_deref() {
            Some("end_turn") => RunOutcome {
                session_id,
                status: RunStatus::Completed,
                error: None,
            },
            Some("cancelled") if self.refused => RunOutcome {
                session_id,
                status: RunStatus::Completed,
                error: None,
            },
            Some("cancelled") => RunOutcome::failed(
                session_id,
                "Grok cancelled the turn although Taceta did not ask it to.",
            ),
            Some(other) => RunOutcome::failed(
                session_id,
                format!("Grok stopped the turn before finishing: {other}."),
            ),
            None => RunOutcome::failed(session_id, "Grok ended the turn without a reason."),
        }
    }

    fn emit_usage(&self, result: &Value) {
        let Some(usage) = result.pointer("/_meta/usage") else {
            return;
        };
        let model_id = result
            .pointer("/_meta/modelId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.chosen_model.clone());
        let context_window = model_id.and_then(|id| {
            self.models
                .iter()
                .find(|model| model.id == id)
                .and_then(|model| model.context_window)
        });
        self.emit(Event::Usage(Usage {
            input_tokens: usage
                .get("inputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            context_window,
        }));
    }

    /// Returns the prompt's response once it arrives.
    async fn on_message(
        &mut self,
        process: &mut CliProcess,
        prompt_id: u64,
        message: Value,
    ) -> Result<Option<Result<Value, String>>, String> {
        match classify(message)? {
            Incoming::Response { id, result } => {
                if id.as_u64() == Some(prompt_id) {
                    return Ok(Some(result));
                }
                // Responses to anything else (the CLI sends some with string
                // ids of its own) are not ours.
                Ok(None)
            }
            Incoming::Notification { method, params } => {
                if method == "session/update" {
                    self.on_update(&params)?;
                }
                // Every other notification, including the CLI's own `x.ai/`
                // ones, carries nothing this driver needs.
                Ok(None)
            }
            Incoming::Request { id, method, params } => {
                if method == "session/request_permission" {
                    self.on_permission_request(process, id, &params).await?;
                } else {
                    process.send(&unsupported_message(&id)).await?;
                }
                Ok(None)
            }
        }
    }

    fn on_update(&mut self, params: &Value) -> Result<(), String> {
        let replayed = params.pointer("/_meta/isReplay").and_then(Value::as_bool) == Some(true);
        if replayed {
            return Ok(());
        }
        let update = params
            .get("update")
            .filter(|update| update.is_object())
            .ok_or("Grok sent a session/update without an update.")?;
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .ok_or("Grok sent a session/update without a sessionUpdate kind.")?;
        match kind {
            "agent_message_chunk" => {
                if let Some(text) = chunk_text(update, kind)? {
                    self.emit(Event::MessageDelta(text));
                }
            }
            "agent_thought_chunk" => {
                if let Some(text) = chunk_text(update, kind)? {
                    self.emit(Event::ThinkingDelta(text));
                }
            }
            "tool_call" | "tool_call_update" => self.merge_tool(update)?,
            // Plans, command lists, session info, replayed user messages, ...
            _ => {}
        }
        Ok(())
    }

    fn merge_tool(&mut self, update: &Value) -> Result<(), String> {
        let id = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .ok_or("Grok sent a tool update without a toolCallId.")?;
        let content = parse_tool_content(update)?;
        let item = self.tools.entry(id.to_owned()).or_insert_with(|| ToolItem {
            id: id.to_owned(),
            title: String::new(),
            detail: String::new(),
            diff: None,
            status: ToolStatus::Running,
        });
        if let Some(title) = update
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            item.title = title.to_owned();
        }
        if item.detail.is_empty() {
            if let Some(raw) = update.get("rawInput") {
                item.detail = raw_input_text(raw);
            }
        }
        if !content.texts.is_empty() {
            item.detail = content.texts.join("\n");
        }
        if !content.diffs.is_empty() {
            item.diff = Some(content.diffs.join("\n\n"));
        }
        match update.get("status").and_then(Value::as_str) {
            Some("completed") => item.status = ToolStatus::Completed,
            Some("failed") => item.status = ToolStatus::Failed,
            Some(_) => item.status = ToolStatus::Running,
            // An update without a status does not undo an earlier outcome.
            None => {}
        }
        if item.title.is_empty() {
            item.title = update
                .get("kind")
                .and_then(Value::as_str)
                .filter(|kind| !kind.is_empty())
                .unwrap_or("Tool")
                .to_owned();
        }
        let event = Event::Tool(item.clone());
        self.emit(event);
        Ok(())
    }

    async fn on_permission_request(
        &mut self,
        process: &mut CliProcess,
        request_id: Value,
        params: &Value,
    ) -> Result<(), String> {
        let options = match parse_permission_options(params) {
            Ok(options) => options,
            Err(error) => {
                process
                    .send(&result_message(&request_id, permission_outcome(None)))
                    .await?;
                return Err(error);
            }
        };
        if self.stop.is_some() || !self.controls_open {
            // Nobody can answer it any more.
            process
                .send(&result_message(&request_id, permission_outcome(None)))
                .await?;
            if self.stop.is_none() {
                self.begin_stop(process, Stop::Interrupted).await;
            }
            return Ok(());
        }
        let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
        let tracked = tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .and_then(|id| self.tools.get(id));
        let request = ApprovalRequest {
            id: Uuid::new_v4(),
            title: approval_title(tool_call, tracked),
            detail: approval_detail(tool_call, tracked),
        };
        self.pending.push(PendingApproval {
            id: request.id,
            request_id,
            options,
        });
        self.emit(Event::Approval(request));
        Ok(())
    }

    async fn answer_approval(
        &mut self,
        process: &mut CliProcess,
        id: Uuid,
        approved: bool,
    ) -> Result<(), String> {
        let Some(position) = self.pending.iter().position(|approval| approval.id == id) else {
            // Not a request this run is waiting on.
            return Ok(());
        };
        let approval = self.pending.remove(position);
        self.refused |= !approved;
        let wanted = if approved {
            "allow_once"
        } else {
            "reject_once"
        };
        let option = approval.options.iter().find(|option| option.kind == wanted);
        process
            .send(&result_message(
                &approval.request_id,
                permission_outcome(option.map(|option| option.id.as_str())),
            ))
            .await?;
        self.emit(Event::ApprovalResolved { id });
        match option {
            Some(_) => Ok(()),
            None => Err(format!(
                "Grok's permission request offered no \"{wanted}\" choice, so Taceta cancelled it."
            )),
        }
    }
}

fn chunk_text(update: &Value, kind: &str) -> Result<Option<String>, String> {
    let content = update
        .get("content")
        .ok_or_else(|| format!("Grok sent a {kind} without content."))?;
    // Only text is shown; other content types (images, ...) are not displayed.
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return Ok(None);
    }
    content
        .get("text")
        .and_then(Value::as_str)
        .map(|text| Some(text.to_owned()))
        .ok_or_else(|| format!("Grok sent a text {kind} without text."))
}

struct ToolContent {
    texts: Vec<String>,
    diffs: Vec<String>,
}

fn parse_tool_content(update: &Value) -> Result<ToolContent, String> {
    let mut content = ToolContent {
        texts: Vec::new(),
        diffs: Vec::new(),
    };
    let Some(items) = update.get("content") else {
        return Ok(content);
    };
    let items = items
        .as_array()
        .ok_or("Grok sent tool content that is not a list.")?;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("content") => {
                let inner = item.get("content");
                if inner
                    .and_then(|inner| inner.get("type"))
                    .and_then(Value::as_str)
                    == Some("text")
                {
                    let text = inner
                        .and_then(|inner| inner.get("text"))
                        .and_then(Value::as_str)
                        .ok_or("Grok sent tool text content without text.")?;
                    if !text.is_empty() {
                        content.texts.push(text.to_owned());
                    }
                }
            }
            Some("diff") => {
                let path = item
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("Grok sent a diff without a path.")?;
                let new_text = item
                    .get("newText")
                    .and_then(Value::as_str)
                    .ok_or("Grok sent a diff without newText.")?;
                let old_text = item.get("oldText").and_then(Value::as_str);
                content.diffs.push(render_diff(path, old_text, new_text));
            }
            _ => {}
        }
    }
    Ok(content)
}

/// A header with the path, then removed lines as `- ` and added lines as
/// `+ `. `old_text` of `None` means the file is new.
fn render_diff(path: &str, old_text: Option<&str>, new_text: &str) -> String {
    let mut lines = vec![match old_text {
        Some(_) => path.to_owned(),
        None => format!("{path} (new file)"),
    }];
    lines.extend(
        old_text
            .unwrap_or_default()
            .lines()
            .map(|line| format!("- {line}")),
    );
    lines.extend(new_text.lines().map(|line| format!("+ {line}")));
    lines.join("\n")
}

fn raw_input_text(raw: &Value) -> String {
    if let Some(command) = raw.get("command").and_then(Value::as_str) {
        return command.to_owned();
    }
    match raw {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn parse_permission_options(params: &Value) -> Result<Vec<PermissionOption>, String> {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .ok_or("Grok sent a permission request without options.")?;
    options
        .iter()
        .map(|option| {
            let id = option.get("optionId").and_then(Value::as_str);
            let kind = option.get("kind").and_then(Value::as_str);
            match (id, kind) {
                (Some(id), Some(kind)) => Ok(PermissionOption {
                    id: id.to_owned(),
                    kind: kind.to_owned(),
                }),
                _ => Err("Grok sent a permission option without optionId or kind.".to_owned()),
            }
        })
        .collect()
}

/// The CLI's request title carries the hook's reason after " — hook"; only
/// the part before it is a title.
fn approval_title(tool_call: &Value, tracked: Option<&ToolItem>) -> String {
    let from_request = tool_call
        .get("title")
        .and_then(Value::as_str)
        .map(|title| title.split(" — hook").next().unwrap_or(title).trim())
        .filter(|title| !title.is_empty());
    if let Some(title) = from_request {
        return title.to_owned();
    }
    match tracked {
        Some(item) if !item.title.is_empty() => item.title.clone(),
        _ => "Grok asks to run an action".to_owned(),
    }
}

/// What will run: the command, then the edit if the pending tool item has one.
fn approval_detail(tool_call: &Value, tracked: Option<&ToolItem>) -> String {
    let raw = tool_call.get("rawInput");
    let mut parts = Vec::new();
    if let Some(command) = raw
        .and_then(|raw| raw.get("command"))
        .and_then(Value::as_str)
    {
        parts.push(command.to_owned());
    }
    if let Some(diff) = tracked.and_then(|item| item.diff.as_deref()) {
        parts.push(diff.to_owned());
    }
    if parts.is_empty() {
        if let Some(item) = tracked.filter(|item| !item.detail.is_empty()) {
            parts.push(item.detail.clone());
        } else if let Some(raw) = raw {
            let text = raw_input_text(raw);
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant as StdInstant;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf},
        task::JoinHandle,
    };

    const TEST_TIMING: Timing = Timing {
        startup: Duration::from_millis(400),
        cancel_grace: Duration::from_millis(300),
        model_refresh: Duration::from_millis(200),
    };

    /// The test plays the CLI: it reads what the driver wrote and writes
    /// scripted lines back.
    struct Peer {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Peer {
        async fn read(&mut self) -> Value {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
                    .await
                    .expect("the driver wrote nothing within 5 seconds");
            assert!(read.unwrap() > 0, "the driver closed its output");
            serde_json::from_str(&line).unwrap()
        }

        async fn write(&mut self, message: Value) {
            let mut line = serde_json::to_vec(&message).unwrap();
            line.push(b'\n');
            self.writer.write_all(&line).await.unwrap();
        }

        async fn respond(&mut self, request: &Value, result: Value) {
            self.write(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
                .await;
        }

        async fn update(&mut self, update: Value) {
            self.write(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "s-1", "update": update, "_meta": {}}
            }))
            .await;
        }

        async fn replayed(&mut self, update: Value) {
            self.write(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "s-1", "update": update, "_meta": {"isReplay": true}}
            }))
            .await;
        }

        async fn permission(&mut self, id: u64, options: Value) {
            self.write(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "s-1",
                    "toolCall": {
                        "toolCallId": "call-1",
                        "kind": "edit",
                        "title": "Write `/ws/a.txt` — hook 'x' asks: y",
                        "rawInput": {"variant": "Write", "path": "/ws/a.txt"}
                    },
                    "options": options
                }
            }))
            .await;
        }
    }

    fn model_state() -> Value {
        json!({
            "currentModelId": "grok-4.7",
            "availableModels": [{
                "modelId": "grok-4.7",
                "name": "Grok 4.7",
                "description": "d",
                "_meta": {
                    "totalContextTokens": 256000,
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "xhigh",
                    "reasoningEfforts": [
                        {"id": "xhigh", "value": "xhigh", "label": "Extra High", "default": false},
                        {"id": "high", "value": "high", "label": "High", "default": true},
                        {"id": "medium", "value": "medium", "label": "Medium", "default": false},
                        {"id": "low", "value": "low", "label": "Low", "default": false}
                    ]
                }
            }]
        })
    }

    fn all_options() -> Value {
        json!([
            {"optionId": "always-allow", "name": "Yes, and don't ask again", "kind": "allow_always"},
            {"optionId": "allow-once", "name": "Yes, proceed", "kind": "allow_once"},
            {"optionId": "reject-once", "name": "No", "kind": "reject_once"}
        ])
    }

    fn request(session_id: Option<&str>) -> RunRequest {
        RunRequest {
            session_id: session_id.map(str::to_owned),
            workspace: PathBuf::from("/ws"),
            model: None,
            effort: None,
            prompt: "do it".to_owned(),
            approval: ApprovalPolicy::FollowCli,
            max_duration_secs: 60,
        }
    }

    struct Fixture {
        peer: Peer,
        events: mpsc::UnboundedReceiver<Event>,
        controls: mpsc::UnboundedSender<Control>,
        cancel: watch::Sender<bool>,
        handle: JoinHandle<RunOutcome>,
    }

    fn start(request: RunRequest) -> Fixture {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (our_read, our_write) = tokio::io::split(ours);
        let (their_read, their_write) = tokio::io::split(theirs);
        let process = CliProcess::from_io(our_read, our_write);
        let (event_sender, events) = mpsc::unbounded_channel();
        let (controls, control_receiver) = mpsc::unbounded_channel();
        let (cancel, cancel_receiver) = watch::channel(false);
        let handle = tokio::spawn(run_session(
            process,
            request,
            event_sender,
            control_receiver,
            cancel_receiver,
            TEST_TIMING,
        ));
        Fixture {
            peer: Peer {
                reader: BufReader::new(their_read),
                writer: their_write,
            },
            events,
            controls,
            cancel,
            handle,
        }
    }

    impl Fixture {
        /// Answers initialize and session/new (or load) and any option
        /// requests, and returns every request the driver wrote, ending with
        /// session/prompt.
        async fn handshake(&mut self, load: bool) -> Vec<Value> {
            let mut seen = Vec::new();
            loop {
                let message = self.peer.read().await;
                seen.push(message.clone());
                match message["method"].as_str().unwrap() {
                    "initialize" => {
                        self.peer
                            .respond(
                                &message,
                                json!({
                                    "protocolVersion": 1,
                                    "agentCapabilities": {"loadSession": true},
                                    "_meta": {"agentVersion": "1.0.31", "modelState": model_state()}
                                }),
                            )
                            .await;
                    }
                    "session/new" => {
                        assert!(!load);
                        self.peer
                            .respond(&message, json!({"sessionId": "s-1", "models": model_state(), "configOptions": []}))
                            .await;
                    }
                    "session/load" => {
                        assert!(load);
                        self.peer.replayed(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "old"}})).await;
                        self.peer.replayed(json!({"sessionUpdate": "tool_call", "toolCallId": "old-call", "title": "old"})).await;
                        self.peer
                            .respond(&message, json!({"models": model_state()}))
                            .await;
                    }
                    "session/set_config_option" => {
                        self.peer.respond(&message, json!([])).await;
                    }
                    "session/prompt" => return seen,
                    other => panic!("unexpected startup request {other}"),
                }
            }
        }

        async fn finish_turn(&mut self, prompt: &Value, stop_reason: &str) {
            self.peer
                .respond(
                    prompt,
                    json!({
                        "stopReason": stop_reason,
                        "_meta": {
                            "usage": {"inputTokens": 12, "outputTokens": 3, "totalTokens": 15},
                            "modelId": "grok-4.7"
                        }
                    }),
                )
                .await;
        }

        async fn events_until_approval(&mut self) -> (Vec<Event>, Uuid) {
            let mut seen = Vec::new();
            loop {
                let event = tokio::time::timeout(Duration::from_secs(5), self.events.recv())
                    .await
                    .expect("no event within 5 seconds")
                    .expect("event channel closed");
                if let Event::Approval(request) = &event {
                    let id = request.id;
                    seen.push(event);
                    return (seen, id);
                }
                seen.push(event);
            }
        }

        async fn outcome(self) -> (RunOutcome, Vec<Event>) {
            let Fixture {
                mut events,
                handle,
                controls,
                cancel,
                peer,
            } = self;
            let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("the run did not finish")
                .unwrap();
            drop((controls, cancel, peer));
            let mut all = Vec::new();
            while let Ok(event) = events.try_recv() {
                all.push(event);
            }
            (outcome, all)
        }
    }

    fn tool(id: &str, title: &str, detail: &str, diff: Option<&str>, status: ToolStatus) -> Event {
        Event::Tool(ToolItem {
            id: id.to_owned(),
            title: title.to_owned(),
            detail: detail.to_owned(),
            diff: diff.map(str::to_owned),
            status,
        })
    }

    #[tokio::test]
    async fn a_new_session_runs_a_full_turn_in_order() {
        let mut request = request(None);
        request.model = Some("grok-4.7".into());
        request.effort = Some("high".into());
        let mut fixture = start(request);
        let seen = fixture.handshake(false).await;

        let methods: Vec<&str> = seen.iter().map(|m| m["method"].as_str().unwrap()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/set_config_option",
                "session/prompt"
            ]
        );
        assert_eq!(seen[0]["params"]["protocolVersion"], 1);
        assert_eq!(seen[0]["params"]["clientInfo"]["name"], "taceta");
        assert_eq!(seen[0]["params"]["clientCapabilities"]["terminal"], false);
        assert_eq!(seen[1]["params"], json!({"cwd": "/ws", "mcpServers": []}));
        assert_eq!(
            seen[2]["params"],
            json!({"sessionId": "s-1", "configId": "model", "value": "grok-4.7"})
        );
        assert_eq!(
            seen[3]["params"],
            json!({"sessionId": "s-1", "configId": "reasoning_effort", "value": "high"})
        );
        assert_eq!(
            seen[4]["params"],
            json!({"sessionId": "s-1", "prompt": [{"type": "text", "text": "do it"}]})
        );
        let ids: Vec<u64> = seen.iter().map(|m| m["id"].as_u64().unwrap()).collect();
        assert_eq!(
            ids.len(),
            ids.iter().collect::<std::collections::HashSet<_>>().len()
        );
        let prompt = seen[4].clone();

        fixture.peer.update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello"}})).await;
        fixture.peer.update(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}})).await;
        fixture
            .peer
            .update(json!({"sessionUpdate": "plan", "entries": []}))
            .await;
        fixture
            .peer
            .update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "write",
                "rawInput": {"path": "/ws/a.txt"}, "status": "pending"
            }))
            .await;
        fixture.peer.update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "in_progress",
            "title": "Write /ws/a.txt",
            "content": [{"type": "diff", "path": "/ws/a.txt", "oldText": "a\nb", "newText": "a\nc"}]
        })).await;
        fixture.peer.permission(77, all_options()).await;

        let (mut events, approval_id) = fixture.events_until_approval().await;
        fixture
            .controls
            .send(Control::Approval {
                id: approval_id,
                approved: true,
            })
            .unwrap();
        let answer = fixture.peer.read().await;
        assert_eq!(
            answer,
            json!({
                "jsonrpc": "2.0", "id": 77,
                "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}
            })
        );

        fixture
            .peer
            .update(json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "wrote it"}}]
            }))
            .await;
        fixture.finish_turn(&prompt, "end_turn").await;

        let (outcome, rest) = fixture.outcome().await;
        events.extend(rest);
        let diff = "/ws/a.txt\n- a\n- b\n+ a\n+ c";
        let detail_before = r#"{"path":"/ws/a.txt"}"#;
        let Event::Approval(approval) = events[5].clone() else {
            panic!("{events:?}")
        };
        assert_eq!(approval.title, "Write `/ws/a.txt`");
        assert_eq!(approval.detail, diff);
        assert_eq!(
            events,
            vec![
                Event::SessionReady {
                    session_id: "s-1".into()
                },
                Event::MessageDelta("Hello".into()),
                Event::ThinkingDelta("hmm".into()),
                tool("call-1", "write", detail_before, None, ToolStatus::Running),
                tool(
                    "call-1",
                    "Write /ws/a.txt",
                    detail_before,
                    Some(diff),
                    ToolStatus::Running
                ),
                Event::Approval(approval),
                Event::ApprovalResolved { id: approval_id },
                tool(
                    "call-1",
                    "Write /ws/a.txt",
                    "wrote it",
                    Some(diff),
                    ToolStatus::Completed
                ),
                Event::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    context_window: Some(256000)
                }),
            ]
        );
        assert_eq!(
            outcome,
            RunOutcome {
                session_id: Some("s-1".into()),
                status: RunStatus::Completed,
                error: None
            }
        );
    }

    #[tokio::test]
    async fn a_loaded_session_emits_nothing_for_replayed_history() {
        let mut fixture = start(request(Some("old-1")));
        let seen = fixture.handshake(true).await;
        assert_eq!(seen[1]["method"], "session/load");
        assert_eq!(
            seen[1]["params"],
            json!({"sessionId": "old-1", "cwd": "/ws", "mcpServers": []})
        );
        let prompt = seen.last().unwrap().clone();
        // Replay that is still arriving after the prompt was sent.
        fixture.peer.replayed(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "old thought"}})).await;
        fixture.peer.update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "new"}})).await;
        fixture.finish_turn(&prompt, "end_turn").await;
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(
            events,
            vec![
                Event::SessionReady {
                    session_id: "old-1".into()
                },
                Event::MessageDelta("new".into()),
                Event::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    context_window: Some(256000)
                }),
            ]
        );
        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.session_id.as_deref(), Some("old-1"));
    }

    #[tokio::test]
    async fn a_denied_approval_selects_the_reject_option_and_never_an_always_option() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture.peer.permission(5, all_options()).await;
        let (_, approval_id) = fixture.events_until_approval().await;
        fixture
            .controls
            .send(Control::Approval {
                id: approval_id,
                approved: false,
            })
            .unwrap();
        let answer = fixture.peer.read().await;
        assert_eq!(answer["id"], 5);
        assert_eq!(
            answer["result"],
            json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}})
        );
        // The CLI ends a turn in which an action was refused as cancelled.
        fixture.finish_turn(&prompt, "cancelled").await;
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(events[0], Event::ApprovalResolved { id: approval_id });
        assert_eq!((outcome.status, outcome.error), (RunStatus::Completed, None));
    }

    #[tokio::test]
    async fn a_missing_option_kind_cancels_the_request_and_fails_the_run() {
        let only_reject = json!([
            {"optionId": "always-allow", "name": "always", "kind": "allow_always"},
            {"optionId": "reject-once", "name": "No", "kind": "reject_once"}
        ]);
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture.peer.permission(5, only_reject).await;
        let (_, approval_id) = fixture.events_until_approval().await;
        fixture
            .controls
            .send(Control::Approval {
                id: approval_id,
                approved: true,
            })
            .unwrap();
        let answer = fixture.peer.read().await;
        assert_eq!(answer["id"], 5);
        assert_eq!(
            answer["result"],
            json!({"outcome": {"outcome": "cancelled"}})
        );
        // The failed run also tells the CLI to cancel the turn.
        let cancel = fixture.peer.read().await;
        assert_eq!(cancel["method"], "session/cancel");
        fixture.finish_turn(&prompt, "cancelled").await;
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        let error = outcome.error.unwrap();
        assert!(error.contains("allow_once"), "{error}");
        assert_eq!(events[0], Event::ApprovalResolved { id: approval_id });
    }

    #[tokio::test]
    async fn cancel_tells_the_cli_answers_pending_requests_and_is_interrupted() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture.peer.permission(9, all_options()).await;
        let (_, approval_id) = fixture.events_until_approval().await;
        fixture.cancel.send(true).unwrap();
        let first = fixture.peer.read().await;
        let second = fixture.peer.read().await;
        let (notification, answer) = if first.get("id").is_none() {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(
            notification,
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s-1"}})
        );
        assert_eq!(answer["id"], 9);
        assert_eq!(
            answer["result"],
            json!({"outcome": {"outcome": "cancelled"}})
        );
        fixture.finish_turn(&prompt, "cancelled").await;
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Interrupted);
        assert_eq!(outcome.error, None);
        assert_eq!(events[0], Event::ApprovalResolved { id: approval_id });
    }

    #[tokio::test]
    async fn a_cli_that_never_answers_a_cancel_is_given_up_on_after_the_grace_period() {
        let mut fixture = start(request(None));
        fixture.handshake(false).await;
        let began = StdInstant::now();
        fixture.cancel.send(true).unwrap();
        let cancel = fixture.peer.read().await;
        assert_eq!(cancel["method"], "session/cancel");
        let (outcome, _) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Interrupted);
        let elapsed = began.elapsed();
        assert!(elapsed >= TEST_TIMING.cancel_grace, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[tokio::test]
    async fn dropping_the_event_receiver_stops_the_run_as_interrupted() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        drop(std::mem::replace(
            &mut fixture.events,
            mpsc::unbounded_channel().1,
        ));
        let cancel = fixture.peer.read().await;
        assert_eq!(cancel["method"], "session/cancel");
        fixture.finish_turn(&prompt, "cancelled").await;
        let (outcome, _) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Interrupted);
    }

    #[tokio::test]
    async fn the_time_limit_counts_approval_waiting_and_ends_as_limit_reached() {
        let mut request = request(None);
        request.max_duration_secs = 1;
        let mut fixture = start(request);
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture.peer.permission(3, all_options()).await;
        let (_, approval_id) = fixture.events_until_approval().await;
        let first = fixture.peer.read().await;
        let second = fixture.peer.read().await;
        let methods = [first["method"].as_str(), second["method"].as_str()];
        assert!(
            methods.contains(&Some("session/cancel")),
            "{first} {second}"
        );
        fixture.finish_turn(&prompt, "cancelled").await;
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::LimitReached);
        assert_eq!(events[0], Event::ApprovalResolved { id: approval_id });
    }

    #[tokio::test]
    async fn output_ending_before_the_response_is_a_failure_not_a_completion() {
        let mut fixture = start(request(None));
        fixture.handshake(false).await;
        fixture.peer.writer.shutdown().await.unwrap();
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(
            outcome.error.as_deref(),
            Some("Grok ended before finishing the turn.")
        );
        assert!(!events.iter().any(|event| matches!(event, Event::Usage(_))));
    }

    #[tokio::test]
    async fn a_jsonrpc_error_on_the_prompt_fails_with_its_message() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture
            .peer
            .write(json!({"jsonrpc": "2.0", "id": prompt["id"], "error": {"code": -32000, "message": "model overloaded"}}))
            .await;
        let (outcome, _) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(outcome.error.as_deref(), Some("model overloaded"));
    }

    #[tokio::test]
    async fn other_stop_reasons_fail_and_name_the_reason() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture.finish_turn(&prompt, "max_tokens").await;
        let (outcome, _) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(outcome.error.unwrap().contains("max_tokens"));
    }

    #[tokio::test]
    async fn a_startup_step_without_an_answer_fails_naming_the_step() {
        let mut fixture = start(request(None));
        let initialize = fixture.peer.read().await;
        assert_eq!(initialize["method"], "initialize");
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(outcome.error.unwrap().contains("initialize"));
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn an_error_while_setting_the_model_fails_the_run_with_the_cli_message() {
        let mut request = request(None);
        request.model = Some("nope".into());
        let mut fixture = start(request);
        loop {
            let message = fixture.peer.read().await;
            match message["method"].as_str().unwrap() {
                "initialize" => {
                    fixture.peer.respond(&message, json!({"agentCapabilities": {"loadSession": true}, "_meta": {"modelState": model_state()}})).await;
                }
                "session/new" => {
                    fixture
                        .peer
                        .respond(&message, json!({"sessionId": "s-1"}))
                        .await;
                }
                "session/set_config_option" => {
                    fixture
                        .peer
                        .write(json!({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "unknown model nope"}}))
                        .await;
                    break;
                }
                other => panic!("{other}"),
            }
        }
        let (outcome, _) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        let error = outcome.error.unwrap();
        assert!(
            error.contains("unknown model nope") && error.contains("set model"),
            "{error}"
        );
        assert_eq!(outcome.session_id.as_deref(), Some("s-1"));
    }

    #[tokio::test]
    async fn strays_and_vendor_notifications_are_ignored_and_unknown_requests_are_refused() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture
            .peer
            .write(json!({"jsonrpc": "2.0", "id": "skills-reload", "result": {}}))
            .await;
        fixture
            .peer
            .write(json!({"jsonrpc": "2.0", "id": 4242, "result": {}}))
            .await;
        fixture.peer.write(json!({"jsonrpc": "2.0", "method": "_x.ai/session_notification",
            "params": {"update": {"sessionUpdate": "turn_completed", "usage": {"inputTokens": 999, "outputTokens": 999}}}})).await;
        fixture
            .peer
            .write(json!({"jsonrpc": "2.0", "method": "x.ai/whatever", "params": {}}))
            .await;
        fixture
            .peer
            .update(json!({"sessionUpdate": "available_commands_update", "availableCommands": []}))
            .await;
        fixture.peer.write(json!({"jsonrpc": "2.0", "id": 31, "method": "fs/read_text_file", "params": {"path": "/x"}})).await;
        let reply = fixture.peer.read().await;
        assert_eq!(
            reply,
            json!({"jsonrpc": "2.0", "id": 31, "error": {"code": -32601, "message": "Taceta does not support this request."}})
        );
        fixture.peer.update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "ok"}})).await;
        fixture.finish_turn(&prompt, "end_turn").await;
        let (outcome, events) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(
            events,
            vec![
                Event::SessionReady {
                    session_id: "s-1".into()
                },
                Event::MessageDelta("ok".into()),
                Event::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    context_window: Some(256000)
                }),
            ]
        );
    }

    #[tokio::test]
    async fn a_malformed_update_fails_the_run_instead_of_being_skipped() {
        let mut fixture = start(request(None));
        let seen = fixture.handshake(false).await;
        let prompt = seen.last().unwrap().clone();
        fixture
            .peer
            .update(json!({"sessionUpdate": "tool_call", "title": "no id"}))
            .await;
        let cancel = fixture.peer.read().await;
        assert_eq!(cancel["method"], "session/cancel");
        fixture.finish_turn(&prompt, "cancelled").await;
        let (outcome, _) = fixture.outcome().await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(outcome.error.unwrap().contains("toolCallId"));
    }

    #[test]
    fn model_state_becomes_model_options_weakest_effort_first() {
        let options = parse_model_options(&model_state()).unwrap();
        assert_eq!(
            options,
            vec![ModelOption {
                id: "grok-4.7".into(),
                label: "Grok 4.7".into(),
                context_window: Some(256000),
                efforts: vec!["low".into(), "medium".into(), "high".into(), "xhigh".into()],
                default_effort: Some("high".into()),
            }]
        );
        let plain = json!({"availableModels": [{"modelId": "m"}]});
        assert_eq!(
            parse_model_options(&plain).unwrap(),
            vec![ModelOption {
                id: "m".into(),
                label: "m".into(),
                context_window: None,
                efforts: vec![],
                default_effort: None
            }]
        );
        assert!(parse_model_options(&json!({})).is_err());
        assert!(parse_model_options(&json!({"availableModels": [{"name": "x"}]})).is_err());
    }

    #[tokio::test]
    async fn the_stored_model_list_is_read_from_the_initialize_answer_without_waiting() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (our_read, our_write) = tokio::io::split(ours);
        let (their_read, their_write) = tokio::io::split(theirs);
        let mut process = CliProcess::from_io(our_read, our_write);
        let mut peer = Peer {
            reader: BufReader::new(their_read),
            writer: their_write,
        };
        let task = tokio::spawn(async move { stored_models(&mut process, TEST_TIMING).await });
        let initialize = peer.read().await;
        assert_eq!(initialize["method"], "initialize");
        peer.respond(&initialize, json!({"_meta": {"modelState": model_state()}}))
            .await;
        let options = task.await.unwrap().unwrap().unwrap();
        assert_eq!(options[0].id, "grok-4.7");
        assert_eq!(options[0].efforts.first().map(String::as_str), Some("low"));
    }

    #[tokio::test]
    async fn the_model_list_announced_after_initialize_is_offered_as_the_refreshed_one() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (our_read, our_write) = tokio::io::split(ours);
        let (their_read, their_write) = tokio::io::split(theirs);
        let mut process = CliProcess::from_io(our_read, our_write);
        let mut peer = Peer {
            reader: BufReader::new(their_read),
            writer: their_write,
        };
        let task = tokio::spawn(async move {
            let stored = stored_models(&mut process, TEST_TIMING).await;
            (stored, announced_models(&mut process, TEST_TIMING).await)
        });
        let initialize = peer.read().await;
        let cached = json!({"availableModels": [{"modelId": "older-model", "name": "Older"}]});
        peer.respond(&initialize, json!({"_meta": {"modelState": cached}})).await;
        peer.write(json!({"jsonrpc": "2.0", "method": "_x.ai/settings/update", "params": {}})).await;
        peer.write(json!({"jsonrpc": "2.0", "method": "_x.ai/models/update", "params": model_state()})).await;
        let (stored, announced) = task.await.unwrap();
        let ids = |options: Vec<ModelOption>| options.into_iter().map(|option| option.id).collect::<Vec<_>>();
        assert_eq!(ids(stored.unwrap().unwrap()), ["older-model"]);
        let announced = ids(announced.unwrap());
        assert!(announced.contains(&"grok-4.7".to_owned()), "{announced:?}");
        assert!(!announced.contains(&"older-model".to_owned()), "{announced:?}");
    }

    #[test]
    fn agent_arguments_load_the_plugin_only_when_every_action_is_asked_about() {
        let dir = Path::new("/support/taceta-grok");
        assert_eq!(
            agent_args(ApprovalPolicy::AskEveryTime, dir),
            ["agent", "--plugin-dir", "/support/taceta-grok", "stdio"].map(OsString::from)
        );
        assert_eq!(
            agent_args(ApprovalPolicy::FollowCli, dir),
            ["agent", "stdio"].map(OsString::from)
        );
    }

    #[test]
    fn diffs_name_the_path_and_mark_removed_added_and_new_files() {
        assert_eq!(
            render_diff("/a", Some("x\ny"), "x\nz"),
            "/a\n- x\n- y\n+ x\n+ z"
        );
        assert_eq!(
            render_diff("/b", None, "one\ntwo\n"),
            "/b (new file)\n+ one\n+ two"
        );
    }

    fn run_hook(script: &Path, input: &str) -> String {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn a_conversation_without_tools_loads_a_plugin_that_refuses_every_call() {
        let root = tempfile::tempdir().unwrap();
        let asking = root.path().join("GrokApprovalPlugin");
        let refusing = refusal_plugin_dir(&asking);
        assert_eq!(refusing, root.path().join("GrokApprovalPluginNoTools"));
        assert_eq!(
            agent_args(ApprovalPolicy::NoTools, &asking),
            [OsString::from("agent"), "--plugin-dir".into(), refusing.clone().into_os_string(), "stdio".into()]
        );
        write_refusal_plugin(&refusing).unwrap();
        // Whatever the tool, the hook answers with a refusal on one line.
        for input in [r#"{"toolName":"Read"}"#, r#"{"toolName":"Bash"}"#, ""] {
            let mut child = std::process::Command::new(refusing.join("hooks/ask.sh"))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), input.as_bytes()).unwrap();
            let output = child.wait_with_output().unwrap();
            let line = String::from_utf8(output.stdout).unwrap();
            assert!(line.ends_with('\n') && line.lines().count() == 1, "{line:?}");
            let decision: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(decision["decision"], "deny");
        }
    }

    #[test]
    fn the_approval_plugin_is_written_privately_and_asks_for_everything_but_reads() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        // The real location has a space in it, and a path may hold a quote.
        let dir = root.path().join("Application Support").join("grok's approval");
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(dir.join("hooks/ask.sh"), "stale").unwrap();
        std::fs::write(dir.join("hooks/hooks.json"), "stale").unwrap();
        write_approval_plugin(&dir).unwrap();
        // Writing again changes nothing.
        write_approval_plugin(&dir).unwrap();

        let script = dir.join("hooks/ask.sh");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("hooks")), 0o700);
        assert_eq!(mode(&script), 0o700);
        let hooks: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("hooks/hooks.json")).unwrap())
                .unwrap();
        let command = hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().to_owned();
        assert_eq!(
            hooks,
            json!({"hooks": {"PreToolUse": [{"hooks": [
                {"type": "command", "command": command, "timeout": 10}
            ]}]}})
        );
        // The CLI hands the command to a shell. It must start the script as written.
        let mut through_shell = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(through_shell.stdin.as_mut().unwrap(), br#"{"toolName":"write"}"#).unwrap();
        let output = through_shell.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            r#"{"decision":"ask","reason":"Taceta asks before every action"}"#
        );

        let ask = r#"{"decision":"ask","reason":"Taceta asks before every action"}"#;
        let defer = r#"{"decision":"defer"}"#;
        let input = |name: &str| {
            format!(
                r#"{{"hookEventName":"PreToolUse","toolName":"{name}","toolInput":{{"command":"ls"}}}}"#
            )
        };
        assert_eq!(run_hook(&script, &input("run_terminal_command")), ask);
        assert_eq!(run_hook(&script, &input("write")), ask);
        for name in READ_ONLY_TOOLS {
            assert_eq!(run_hook(&script, &input(name)), defer, "{name}");
        }
        assert_eq!(run_hook(&script, &input("read_file_but_not_really")), ask);
        assert_eq!(run_hook(&script, "not json at all"), ask);
        assert_eq!(run_hook(&script, ""), ask);
        // A tool argument that merely mentions a read-only tool does not defer.
        let sneaky = r#"{"toolName":"run_terminal_command","toolInput":{"command":"echo \"toolName\":\"read_file\""}}"#;
        assert_eq!(run_hook(&script, sneaky), ask);
    }

    #[test]
    fn a_relative_plugin_directory_is_refused() {
        assert!(write_approval_plugin(Path::new("relative/dir")).is_err());
    }

    #[test]
    fn approval_titles_drop_the_hook_reason() {
        let call = json!({"title": "Execute `touch x` — hook 'h' asks: why", "rawInput": {"command": "touch x"}});
        assert_eq!(approval_title(&call, None), "Execute `touch x`");
        assert_eq!(approval_detail(&call, None), "touch x");
        assert_eq!(
            approval_title(&json!({}), None),
            "Grok asks to run an action"
        );
    }
}
