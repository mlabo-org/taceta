//! Driver for the official Codex CLI's App Server (`codex app-server`).
//!
//! The App Server speaks JSON-RPC, one object per line on stdio. Codex owns
//! its thread, its tools and its compaction; this file turns that protocol
//! into the types in [`super`]. The user signs in with the CLI itself: the
//! driver uses the user's normal `~/.codex` login and settings and never
//! handles credentials.
mod catalog;
mod protocol;
mod transport;

use super::{
    ApprovalPolicy, CliAgent, CliFuture, Control, Event, ModelOption, Readiness, RunOutcome,
    RunRequest, RunStatus,
    process::{find_executable, usual_install_dirs},
};
use catalog::{models, require_account};
use protocol::{Asked, Effect, RunState, required_string};
use serde_json::{Value, json};
use std::{path::{Path, PathBuf}, process::Stdio, sync::Arc};
use tokio::{
    process::Command,
    sync::{Mutex, mpsc::{UnboundedReceiver, UnboundedSender}, watch},
    time::{Duration, Instant, sleep_until, timeout, timeout_at},
};
use transport::{Connection, WireMessage};

const RPC_SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const NOT_FOUND: &str = "The Codex CLI (`codex`) was not found on PATH or in the usual install locations. Install it and sign in with `codex login`.";

/// One operation at a time. Between turns the App Server that ran the last
/// one is kept here, because a new process answers its first turn seconds
/// later than one that is already running.
#[derive(Clone, Default)]
pub struct CodexCli { coordination: Arc<Mutex<Option<Kept>>> }

struct Kept { connection: Connection, served: Served }

/// What the kept App Server last ran. A thread it still holds is rejoined
/// as it stands: a model or permissions asked for on resuming it are not
/// applied, so only an identical request may use the same server again.
#[derive(Clone, PartialEq, Eq)]
struct Served { thread_id: String, cwd: PathBuf, model: Option<String>, approval: ApprovalPolicy }

impl Served {
    fn serves(&self, request: &RunRequest, cwd: &Path) -> bool {
        let no_tools = |approval| approval == ApprovalPolicy::NoTools;
        // The server itself is started differently for a conversation without tools.
        no_tools(self.approval) == no_tools(request.approval)
            && match &request.session_id {
                // A new thread takes what it is started with.
                None => true,
                Some(thread_id) => {
                    *thread_id == self.thread_id && cwd == self.cwd && request.model == self.model && request.approval == self.approval
                }
            }
    }
}

/// Ends the kept App Server. Everything except a turn starts from a new one.
async fn release(kept: &mut Option<Kept>) {
    if let Some(mut kept) = kept.take() { kept.connection.shutdown().await; }
}

impl CodexCli {
    /// Construction does not read files, start a process, or connect to an account.
    pub fn new() -> Self { Self::default() }
}

impl CliAgent for CodexCli {
    fn name(&self) -> &'static str { "Codex" }

    fn readiness(&self) -> CliFuture<Result<Readiness, String>> {
        Box::pin(async {
            let executable = locate()?;
            let version = probe(&executable, &["--version"]).await.and_then(|(_, output)| {
                output.lines().next().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned)
            });
            // `codex login status` exits with success only when signed in.
            let signed_in = probe(&executable, &["login", "status"]).await.map(|(succeeded, _)| succeeded);
            Ok(Readiness { executable, version, signed_in })
        })
    }

    fn models(&self) -> CliFuture<Result<Vec<ModelOption>, String>> {
        let coordination = self.coordination.clone();
        Box::pin(async move {
            timeout(RPC_SESSION_TIMEOUT, async {
                let mut guard = timeout(Duration::from_secs(30), coordination.lock()).await
                    .map_err(|_| "Codex is busy. Wait for or stop its active operation before refreshing models.")?;
                release(&mut guard).await;
                let mut connection = open(false).await?;
                let listed = async {
                    require_account(&mut connection).await?;
                    Ok(models(&mut connection).await?.iter().map(|model| model.option()).collect())
                }.await;
                connection.shutdown().await;
                listed
            }).await.map_err(|_| "Listing Codex's models timed out; no partial list was accepted.".to_owned())?
        })
    }

    fn accepts_attachments(&self) -> bool { true }

    fn run(
        &self, request: RunRequest, events: UnboundedSender<Event>,
        mut controls: UnboundedReceiver<Control>, mut cancel: watch::Receiver<bool>,
    ) -> CliFuture<RunOutcome> {
        let coordination = self.coordination.clone();
        Box::pin(async move {
            let before_turn = |status, message: &str| RunOutcome { session_id: request.session_id.clone(), status, error: Some(message.into()) };
            if request.max_duration_secs == 0 { return before_turn(RunStatus::Failed, "The time limit must be greater than zero."); }
            if request.session_id.as_deref().is_some_and(|id| id.trim().is_empty()) { return before_turn(RunStatus::Failed, "The saved Codex conversation ID is empty."); }
            let Some(deadline) = Instant::now().checked_add(Duration::from_secs(request.max_duration_secs)) else {
                return before_turn(RunStatus::Failed, "The time limit is too large.");
            };
            let mut guard = tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => return before_turn(RunStatus::Interrupted, "The run was cancelled."),
                _ = events.closed() => return before_turn(RunStatus::Interrupted, "The view of this run was closed."),
                _ = sleep_until(deadline) => return before_turn(RunStatus::LimitReached, "The time limit was reached before execution."),
                guard = coordination.lock() => guard,
            };
            let no_tools = request.approval == ApprovalPolicy::NoTools;
            let kept = guard.take();
            let setup = async {
                let cwd = tokio::fs::canonicalize(&request.workspace).await.map_err(|error| format!("Could not open the workspace: {error}"))?;
                if !tokio::fs::metadata(&cwd).await.map_err(|error| error.to_string())?.is_dir() { return Err("The workspace must be a directory.".to_owned()); }
                // The kept App Server is used again when it can serve this
                // request, nothing unexpected arrived while it was idle, and
                // it still answers. Otherwise it is ended and a new one starts.
                let mut reused = None;
                if let Some(mut kept) = kept {
                    if kept.served.serves(&request, &cwd) && kept.connection.discard_idle() && require_account(&mut kept.connection).await.is_ok() {
                        kept.connection.discard_idle();
                        reused = Some(kept.connection);
                    } else {
                        kept.connection.shutdown().await;
                    }
                }
                let account_confirmed = reused.is_some();
                let mut connection = match reused {
                    Some(connection) => connection,
                    None => open(no_tools).await?,
                };
                match prepare(&mut connection, &request, &cwd, account_confirmed).await {
                    Ok(prepared) => Ok((connection, cwd, prepared)),
                    Err(error) => { connection.shutdown().await; Err(error) }
                }
            };
            let (mut connection, cwd, prepared) = tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => return before_turn(RunStatus::Interrupted, "The run was cancelled before execution."),
                _ = events.closed() => return before_turn(RunStatus::Interrupted, "The view of this run was closed."),
                _ = sleep_until(deadline) => return before_turn(RunStatus::LimitReached, "The time limit was reached before execution."),
                result = setup => match result {
                    Ok(ready) => ready,
                    Err(error) => return before_turn(RunStatus::Failed, &error),
                },
            };
            let session_id = Some(prepared.thread_id.clone());
            let served = Served { thread_id: prepared.thread_id.clone(), cwd: cwd.clone(), model: request.model.clone(), approval: request.approval };
            let _ = events.send(Event::SessionReady { session_id: prepared.thread_id.clone() });
            let mut state = RunState::new(prepared.thread_id, &request, events.clone());
            let result = match cwd.to_str() {
                Some(cwd) => execute(&mut connection, &mut state, &request, cwd, prepared.input, &events, &mut controls, &mut cancel, deadline).await,
                None => Err("The workspace path is not valid UTF-8.".into()),
            };
            state.resolve_all();
            // Only a turn that ran to its end leaves the App Server in a known
            // state worth keeping for the next one.
            if matches!(result, Ok((RunStatus::Completed, None))) { *guard = Some(Kept { connection, served }); }
            else { connection.shutdown().await; }
            match result {
                Ok((status, error)) => RunOutcome { session_id, status, error },
                Err(error) => RunOutcome { session_id, status: RunStatus::Failed, error: Some(error) },
            }
        })
    }
}

struct Prepared { thread_id: String, input: Vec<Value> }

/// Checks the account and the chosen model, then starts or resumes the
/// thread and confirms that Codex took what was asked for.
async fn prepare(connection: &mut Connection, request: &RunRequest, cwd: &Path, account_confirmed: bool) -> Result<Prepared, String> {
    if !account_confirmed { require_account(connection).await?; }
    let mut images = false;
    if let Some(model) = &request.model {
        let catalog = models(connection).await?;
        let selected = catalog.iter().find(|entry| &entry.model == model)
            .ok_or("The selected GPT model is no longer offered to this account. Refresh models and select one.")?;
        if let Some(effort) = &request.effort {
            if !selected.supports_effort(effort) { return Err(format!("{model} does not advertise reasoning effort '{effort}'. Select an available effort.")); }
        }
        images = selected.images();
    }
    let input = protocol::input(&request.prompt, &request.attachments, images)?;
    let cwd = cwd.to_str().ok_or("The workspace path is not valid UTF-8.")?;
    let mcp_servers = match request.approval {
        ApprovalPolicy::NoTools => protocol::mcp_server_names(&connection.call("config/read", json!({"cwd":cwd})).await?)?,
        _ => Vec::new(),
    };
    let method = if request.session_id.is_some() { "thread/resume" } else { "thread/start" };
    let response = connection.call(method, protocol::thread_params(request, cwd, &mcp_servers)).await?;
    if request.model.as_deref().is_some_and(|model| response.get("model").and_then(Value::as_str) != Some(model)) {
        return Err("Codex did not accept the selected GPT model. No work was started.".into());
    }
    if response.get("modelProvider").and_then(Value::as_str) != Some("openai") {
        return Err("Codex selected a provider other than OpenAI. No work was started.".into());
    }
    if response.get("cwd").and_then(Value::as_str) != Some(cwd) {
        return Err("Codex did not accept the selected working directory. No work was started.".into());
    }
    protocol::validate_effective_permissions(&response, request.approval, cwd)?;
    let thread = response.get("thread").ok_or("Codex did not return a conversation.")?;
    let thread_id = required_string(thread, "id")?.to_owned();
    if thread_id.is_empty() || request.session_id.as_deref().is_some_and(|id| id != thread_id) {
        return Err("Codex returned an unexpected conversation ID. No work was started.".into());
    }
    Ok(Prepared { thread_id, input })
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    connection: &mut Connection, state: &mut RunState, request: &RunRequest, cwd: &str,
    input: Vec<Value>, events: &UnboundedSender<Event>,
    controls: &mut UnboundedReceiver<Control>, cancel: &mut watch::Receiver<bool>, deadline: Instant,
) -> Result<(RunStatus, Option<String>), String> {
    let params = protocol::turn_params(&state.thread_id, request, cwd, input);
    let start_id = timeout_at(deadline, connection.request("turn/start", params)).await.map_err(|_| "The time limit was reached while starting the turn; its effect is unknown.")??;
    let mut start_pending = true;
    let mut stop: Option<(RunStatus, String)> = None;
    let mut interrupt_id = None;
    let mut stop_deadline = None;
    loop {
        if stop.is_some() && stop_deadline.is_none() { stop_deadline = Some(Instant::now() + INTERRUPT_GRACE); }
        if stop.is_some() && interrupt_id.is_none() {
            if let Some(turn_id) = &state.turn_id {
                let sending = connection.request("turn/interrupt", json!({"threadId":state.thread_id,"turnId":turn_id}));
                interrupt_id = Some(timeout(INTERRUPT_GRACE, sending).await.map_err(|_| "Timed out while interrupting the Codex turn.")??);
            }
        }
        let wait_deadline = stop_deadline.unwrap_or(deadline);
        tokio::select! {
            biased;
            _ = cancelled(cancel), if stop.is_none() => stop = Some((RunStatus::Interrupted, "The run was stopped by the user.".into())),
            _ = events.closed(), if stop.is_none() => stop = Some((RunStatus::Interrupted, "The view of this run was closed.".into())),
            _ = sleep_until(wait_deadline) => {
                if let Some((status, message)) = stop { return Ok((status, Some(format!("{message} The Codex process was ended after the stop grace period.")))); }
                stop = Some((RunStatus::LimitReached, "The time limit was reached.".into()));
            },
            control = controls.recv(), if stop.is_none() => match control {
                Some(control) => match state.control(control) {
                    Ok(Some((local_id, wire_id, response))) => {
                        if let Err(error) = connection.respond(wire_id, response).await { stop = Some((RunStatus::Failed, error)); }
                        let _ = events.send(Event::ApprovalResolved { id: local_id });
                    }
                    Ok(None) => {},
                    Err(error) => stop = Some((RunStatus::Failed, error)),
                },
                None => stop = Some((RunStatus::Interrupted, "The control channel of this run was closed.".into())),
            },
            message = connection.next() => {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => return if let Some((status, message)) = stop { Ok((status, Some(format!("{message} {error}")))) } else { Err(error) },
                };
                match message {
                    WireMessage::Response { id, result } if start_pending && id == start_id => {
                        start_pending = false;
                        match result {
                            Ok(response) => state.set_turn(response.get("turn").ok_or("Codex turn/start response has no turn.")?)?,
                            Err(error) => return if let Some((status, message)) = stop { Ok((status, Some(format!("{message} {error}")))) } else { Err(error) },
                        }
                    }
                    WireMessage::Response { id, result } if interrupt_id.as_ref() == Some(&id) => {
                        if let Err(error) = result { if let Some((_, message)) = &mut stop { message.push_str(&format!(" Interrupt response: {error}")); } }
                    }
                    WireMessage::Response { .. } => { if stop.is_none() { stop = Some((RunStatus::Failed, "Codex returned an unknown response ID.".into())); } }
                    WireMessage::Request { id, method, params } => {
                        if stop.is_some() { let _ = connection.reject(id, "Taceta is stopping this turn.").await; }
                        else {
                            match state.server_request(id.clone(), &method, &params) {
                                Ok(Asked::User) => {}
                                Ok(Asked::Answered(answer)) => { if let Err(error) = connection.respond(id, answer).await { stop = Some((RunStatus::Failed, error)); } }
                                Err(error) => { let _ = connection.reject(id, &error).await; stop = Some((RunStatus::Failed, error)); }
                            }
                        }
                    }
                    WireMessage::Notification { method, params } => match state.notification(&method, &params) {
                        Ok(Effect::Terminal(status, error)) => return if let Some((status, message)) = stop { Ok((status, Some(message))) } else { Ok((status, error)) },
                        Ok(Effect::Continue) => {},
                        Err(error) => { if stop.is_none() { stop = Some((RunStatus::Failed, error)); } }
                    }
                }
            }
        }
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop { if *cancel.borrow_and_update() { return; } if cancel.changed().await.is_err() { return; } }
}

fn locate() -> Result<PathBuf, String> {
    find_executable("codex", &usual_install_dirs()).ok_or_else(|| NOT_FOUND.to_owned())
}

/// Starts an App Server ready for requests. It is started in the user's
/// home; each thread names the folder it works in.
async fn open(no_tools: bool) -> Result<Connection, String> {
    let executable = locate()?;
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_absolute())
        .ok_or("The macOS user home directory is unavailable.")?;
    let mut connection = Connection::spawn(&executable, &home, no_tools).await?;
    if let Err(error) = connection.initialize().await {
        connection.shutdown().await;
        return Err(error);
    }
    Ok(connection)
}

/// Runs a short, inference-free subcommand. Returns whether it succeeded and
/// what it printed on standard output.
async fn probe(executable: &Path, args: &[&str]) -> Option<(bool, String)> {
    let mut command = Command::new(executable);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let output = timeout(PROBE_TIMEOUT, command.output()).await.ok()?.ok()?;
    Some((output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::tests::request;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};
    use tokio::sync::mpsc;

    #[test]
    fn a_kept_server_is_used_again_only_for_a_request_it_would_answer_the_same_way() {
        let served = Served { thread_id: "thread".into(), cwd: "/workspace".into(), model: Some("model".into()), approval: ApprovalPolicy::AskEveryTime };
        let cwd = Path::new("/workspace");
        let again = || RunRequest { session_id: Some("thread".into()), ..request(ApprovalPolicy::AskEveryTime) };
        assert!(served.serves(&again(), cwd));
        // A thread the server holds keeps its model and permissions on resume.
        assert!(!served.serves(&RunRequest { model: Some("other-model".into()), ..again() }, cwd));
        assert!(!served.serves(&RunRequest { approval: ApprovalPolicy::FollowCli, ..again() }, cwd));
        assert!(!served.serves(&again(), Path::new("/elsewhere")));
        assert!(!served.serves(&RunRequest { session_id: Some("another".into()), ..again() }, cwd));
        // A new thread takes what it is started with, unless the server was
        // started for the other kind of conversation.
        assert!(served.serves(&RunRequest { model: Some("other-model".into()), ..request(ApprovalPolicy::FollowCli) }, cwd));
        assert!(!served.serves(&request(ApprovalPolicy::NoTools), cwd));
    }

    #[tokio::test]
    async fn stopping_interrupts_the_running_turn_and_waits_for_its_end() {
        let (client,server)=duplex(8192);let (read,write)=split(client);let mut connection=Connection::from_io(read,write);
        let (events,_rx)=mpsc::unbounded_channel();let (_control_tx,mut controls)=mpsc::unbounded_channel();let (cancel_tx,mut cancel)=watch::channel(false);
        let req=request(ApprovalPolicy::AskEveryTime);let mut state=RunState::new("thread".into(),&req,events.clone());
        let peer=tokio::spawn(async move{
            let (read,mut write)=split(server);let mut read=BufReader::new(read);let mut line=String::new();
            read.read_line(&mut line).await.unwrap();let start:Value=serde_json::from_str(&line).unwrap();
            assert_eq!(start["method"],"turn/start");
            write.write_all(format!("{}\n",json!({"id":start["id"],"result":{"turn":{"id":"turn","status":"inProgress","items":[]}}})).as_bytes()).await.unwrap();
            cancel_tx.send(true).unwrap();line.clear();read.read_line(&mut line).await.unwrap();let interrupt:Value=serde_json::from_str(&line).unwrap();
            assert_eq!(interrupt["method"],"turn/interrupt");assert_eq!(interrupt["params"],json!({"threadId":"thread","turnId":"turn"}));
            write.write_all(format!("{}\n{}\n",json!({"id":interrupt["id"],"result":{}}),json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"interrupted","items":[]}}})).as_bytes()).await.unwrap();
        });
        let result=execute(&mut connection,&mut state,&req,"/workspace",vec![json!({"type":"text","text":"work"})],&events,&mut controls,&mut cancel,Instant::now()+Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.0,RunStatus::Interrupted);peer.await.unwrap();
    }

    #[tokio::test]
    async fn a_connection_that_ends_mid_turn_is_a_failure_not_a_completed_turn() {
        let (client,server)=duplex(8192);let (read,write)=split(client);let mut connection=Connection::from_io(read,write);
        let (events,_rx)=mpsc::unbounded_channel();let (_control_tx,mut controls)=mpsc::unbounded_channel();let (_cancel_tx,mut cancel)=watch::channel(false);
        let req=request(ApprovalPolicy::AskEveryTime);let mut state=RunState::new("thread".into(),&req,events.clone());
        let peer=tokio::spawn(async move{
            let (read,mut write)=split(server);let mut line=String::new();BufReader::new(read).read_line(&mut line).await.unwrap();let start:Value=serde_json::from_str(&line).unwrap();
            write.write_all(format!("{}\n",json!({"id":start["id"],"result":{"turn":{"id":"turn","status":"inProgress","items":[]}}})).as_bytes()).await.unwrap();
        });
        assert!(execute(&mut connection,&mut state,&req,"/workspace",vec![],&events,&mut controls,&mut cancel,Instant::now()+Duration::from_secs(5)).await.is_err());
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn a_thread_is_started_only_after_codex_takes_the_model_folder_and_permissions() {
        let (client,server)=duplex(16384);let (read,write)=split(client);let mut connection=Connection::from_io(read,write);
        let req=request(ApprovalPolicy::AskEveryTime);
        let peer=tokio::spawn(async move{
            let (read,mut write)=split(server);let mut read=BufReader::new(read);
            for _ in 0..3 {
                let mut line=String::new();read.read_line(&mut line).await.unwrap();let call:Value=serde_json::from_str(&line).unwrap();
                let result=match call["method"].as_str().unwrap() {
                    "account/read"=>json!({"account":{"type":"chatgpt"}}),
                    "model/list"=>json!({"data":[{"model":"model","inputModalities":["text"]}]}),
                    "thread/start"=>{
                        assert_eq!(call["params"]["approvalPolicy"],"untrusted");
                        json!({"model":"model","modelProvider":"openai","cwd":"/workspace","approvalPolicy":"untrusted","approvalsReviewer":"user",
                            "sandbox":{"type":"workspaceWrite","writableRoots":["/workspace"],"networkAccess":true,"excludeSlashTmp":true,"excludeTmpdirEnvVar":true},
                            "thread":{"id":"thread"}})
                    }
                    other=>panic!("unexpected {other}"),
                };
                write.write_all(format!("{}\n",json!({"id":call["id"],"result":result})).as_bytes()).await.unwrap();
            }
        });
        // Codex reports network access on, which was not asked for.
        let error=prepare(&mut connection,&req,Path::new("/workspace"),false).await.err().unwrap();
        assert!(error.contains("network access"),"{error}");
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn a_conversation_without_tools_starts_its_thread_with_every_configured_mcp_server_off() {
        let (client,server)=duplex(16384);let (read,write)=split(client);let mut connection=Connection::from_io(read,write);
        let req=request(ApprovalPolicy::NoTools);
        let peer=tokio::spawn(async move{
            let (read,mut write)=split(server);let mut read=BufReader::new(read);
            for _ in 0..4 {
                let mut line=String::new();read.read_line(&mut line).await.unwrap();let call:Value=serde_json::from_str(&line).unwrap();
                let result=match call["method"].as_str().unwrap() {
                    "account/read"=>json!({"account":{"type":"chatgpt"}}),
                    "model/list"=>json!({"data":[{"model":"model","inputModalities":["text"]}]}),
                    "config/read"=>{
                        assert_eq!(call["params"]["cwd"],"/workspace");
                        json!({"config":{"mcp_servers":{"node_repl":{"command":"node"},"byteglass":{"command":"byteglass"}}}})
                    }
                    "thread/start"=>{
                        assert_eq!(call["params"]["config"]["mcp_servers.node_repl.enabled"],false);
                        assert_eq!(call["params"]["config"]["mcp_servers.byteglass.enabled"],false);
                        json!({"model":"model","modelProvider":"openai","cwd":"/workspace","approvalPolicy":"never","approvalsReviewer":"user",
                            "sandbox":{"type":"readOnly","networkAccess":false},"thread":{"id":"thread"}})
                    }
                    other=>panic!("unexpected {other}"),
                };
                write.write_all(format!("{}\n",json!({"id":call["id"],"result":result})).as_bytes()).await.unwrap();
            }
        });
        let prepared=prepare(&mut connection,&req,Path::new("/workspace"),false).await.unwrap();
        assert_eq!(prepared.thread_id,"thread");
        peer.await.unwrap();
    }
}
