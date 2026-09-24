//! ChatGPT OAuth and Codex's complete execution lifecycle. Codex owns its
//! transcript, compaction and tools; this module does not implement AgentModel.
mod catalog;
mod handoff;
mod protocol;
mod transport;
mod types;

pub use types::*;

use crate::domain::ModelDescriptor;
use catalog::{account, models, require_account};
use protocol::{Effect, RunState, Transfer, required_string};
use serde_json::{Value, json};
use std::{collections::HashSet, path::{Path, PathBuf}, sync::Arc};
use tokio::{
    sync::{Mutex, mpsc::{UnboundedReceiver, UnboundedSender}, watch},
    time::{Duration, Instant, sleep_until, timeout, timeout_at},
};
use transport::{Connection, WireMessage};

const RPC_SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Default)]
pub struct GptClient { coordination: Arc<Mutex<()>> }

struct Paths { home: PathBuf, chat: PathBuf, executable: PathBuf }

impl GptClient {
    /// Construction does not read files, start a process, or connect to an account.
    pub fn new() -> Self { Self::default() }

    /// Export only saved public work items. This never authenticates, discovers
    /// models, resumes a thread, starts a turn, or runs a tool.
    pub async fn export_handoff(
        &self, thread_id: &str, expected_workspace: &Path,
    ) -> Result<crate::agent::ExternalWorkHandoff, String> {
        if thread_id.trim().is_empty() { return Err("No saved GPT conversation was selected for transfer.".into()); }
        let _guard = self.coordination.try_lock()
            .map_err(|_| "GPT is still busy. Stop its active operation before transferring the task.")?;
        let mut connection = timeout(RPC_SESSION_TIMEOUT, open(true)).await
            .map_err(|_| "Opening the public Codex history connection timed out.")??;
        let result = timeout(RPC_SESSION_TIMEOUT, handoff::read(&mut connection, thread_id, expected_workspace)).await
            .map_err(|_| "Reading the complete GPT history timed out. No partial task handoff was returned.".to_owned())?;
        connection.shutdown().await;
        result
    }

    pub async fn sign_in(&self, events: UnboundedSender<GptLoginEvent>) -> Result<(), String> {
        tokio::select! {
            biased;
            _ = events.closed() => Err("GPT sign-in was cancelled.".into()),
            result = timeout(LOGIN_TIMEOUT, async {
                let _guard = timeout(Duration::from_secs(30),self.coordination.lock()).await
                    .map_err(|_|"GPT is busy. Stop the active GPT operation before signing in.")?;
                let mut connection = open(true).await?;
                if account(&mut connection).await?.is_some() {
                    let _ = events.send(GptLoginEvent::Progress("ChatGPT is already connected.".into()));
                    connection.shutdown().await;
                    return Ok(());
                }
                let login = connection.call("account/login/start",json!({"type":"chatgpt"})).await?;
                if required_string(&login,"type")? != "chatgpt" { return Err("Codex did not start a ChatGPT OAuth login.".into()); }
                let login_id=required_string(&login,"loginId")?.to_owned();
                let auth_url=required_string(&login,"authUrl")?.to_owned();
                let parsed=url::Url::parse(&auth_url).map_err(|_|"Codex returned an invalid sign-in URL.")?;
                if parsed.scheme()!="https" || !parsed.host_str().is_some_and(|host|host=="chatgpt.com" || host=="openai.com" || host.ends_with(".openai.com")) {
                    return Err("Codex returned a sign-in URL outside OpenAI's authentication service.".into());
                }
                events.send(GptLoginEvent::OpenBrowser(auth_url)).map_err(|_|"GPT sign-in was cancelled.")?;
                events.send(GptLoginEvent::Progress("Complete ChatGPT sign-in in your browser.".into())).map_err(|_|"GPT sign-in was cancelled.")?;
                loop {
                    match connection.next().await? {
                        WireMessage::Notification{method,params} if method=="account/login/completed" && params.get("loginId").and_then(Value::as_str)==Some(login_id.as_str()) => {
                            if params.get("success").and_then(Value::as_bool)!=Some(true) {
                                return Err(params.get("error").and_then(Value::as_str).unwrap_or("ChatGPT sign-in failed.").into());
                            }
                            require_account(&mut connection).await?;
                            connection.shutdown().await;
                            return Ok(());
                        }
                        WireMessage::Request{id,method,..} => { connection.reject(id,"Taceta cannot handle this request during sign-in.").await?; return Err(format!("Unexpected request during ChatGPT sign-in: {method}")); }
                        _ => {}
                    }
                }
            }) => result.map_err(|_|"GPT sign-in timed out. Start sign-in again when ready.".to_owned())?,
        }
    }

    pub async fn sign_out(&self) -> Result<(), String> {
        timeout(RPC_SESSION_TIMEOUT,async {
            let _guard=timeout(Duration::from_secs(30),self.coordination.lock()).await.map_err(|_|"GPT is busy. Stop its active operation before signing out.")?;
            let mut connection=open(true).await?;
            if account(&mut connection).await?.is_some() { connection.call("account/logout",json!({})).await?; }
            connection.shutdown().await;
            Ok(())
        }).await.map_err(|_|"GPT sign-out timed out.".to_owned())?
    }

    pub async fn list_models(&self) -> Result<Vec<ModelDescriptor>, String> {
        timeout(RPC_SESSION_TIMEOUT,async {
            let _guard=timeout(Duration::from_secs(30),self.coordination.lock()).await.map_err(|_|"GPT is busy. Wait for or stop its active operation before refreshing models.")?;
            let mut connection=open(true).await?;
            require_account(&mut connection).await?;
            let catalog=models(&mut connection).await?;
            let descriptors=catalog.iter().map(|model|model.descriptor()).collect();
            connection.shutdown().await;
            Ok(descriptors)
        }).await.map_err(|_|"GPT model discovery timed out; no partial catalog was accepted.".to_owned())?
    }

    /// Call only from the explicit conversation deletion/archive path.
    pub async fn archive_thread(&self, thread_id: &str) -> Result<(), String> {
        if thread_id.trim().is_empty() { return Err("No GPT conversation ID was supplied.".into()); }
        timeout(RPC_SESSION_TIMEOUT,async {
            let _guard=timeout(Duration::from_secs(30),self.coordination.lock()).await.map_err(|_|"GPT is busy. Stop the active operation before archiving its conversation.")?;
            let mut connection=open(true).await?;
            require_account(&mut connection).await?;
            connection.call("thread/archive",json!({"threadId":thread_id})).await?;
            connection.shutdown().await;
            Ok(())
        }).await.map_err(|_|"GPT conversation archival timed out. Its result is unknown; it was not retried.".to_owned())?
    }

    pub async fn run(
        &self, request: GptRunRequest, events: UnboundedSender<GptEvent>,
        mut controls: UnboundedReceiver<GptControl>, mut cancel: watch::Receiver<bool>,
    ) -> Result<GptRunOutcome, String> {
        validate_request(&request)?;
        let deadline=Instant::now().checked_add(Duration::from_secs(request.max_duration_secs)).ok_or("The GPT time limit is too large.")?;
        let _guard=tokio::select! {
            biased;
            _=cancelled(&mut cancel)=>return Ok(stopped_before_turn(&request,GptRunStatus::Interrupted,"GPT operation was cancelled.")),
            _=events.closed()=>return Ok(stopped_before_turn(&request,GptRunStatus::Interrupted,"The GPT view was closed.")),
            _=sleep_until(deadline)=>return Ok(stopped_before_turn(&request,GptRunStatus::LimitReached,"The GPT time limit was reached before execution.")),
            guard=self.coordination.lock()=>guard,
        };
        if controls.is_closed() { return Ok(stopped_before_turn(&request,GptRunStatus::Interrupted,"The GPT control channel was closed.")); }
        let setup=async {
            let paths=paths().await?;
            let cwd=match request.mode {
                GptMode::Chat=>paths.chat.clone(),
                GptMode::Coding=>{
                    let path=request.workspace.as_ref().ok_or("Choose a workspace for GPT coding.")?;
                    let path=tokio::fs::canonicalize(path).await.map_err(|error|format!("Could not open the GPT workspace: {error}"))?;
                    if !tokio::fs::metadata(&path).await.map_err(|error|error.to_string())?.is_dir() { return Err("The GPT workspace must be a directory.".into()); }
                    path
                }
            };
            let mut connection=Connection::spawn(&paths.executable,&paths.home,&paths.chat,request.mode==GptMode::Chat).await?;
            connection.initialize().await?;
            let prepared=prepare(&mut connection,&request,&cwd).await?;
            Ok::<_,String>((connection,cwd,prepared))
        };
        let (mut connection,cwd,prepared)=tokio::select! {
            biased;
            _=cancelled(&mut cancel)=>return Ok(stopped_before_turn(&request,GptRunStatus::Interrupted,"GPT operation was cancelled before execution.")),
            _=events.closed()=>return Ok(stopped_before_turn(&request,GptRunStatus::Interrupted,"The GPT view was closed.")),
            _=sleep_until(deadline)=>return Ok(stopped_before_turn(&request,GptRunStatus::LimitReached,"The GPT time limit was reached before execution.")),
            result=setup=>result?,
        };
        events.send(GptEvent::ThreadReady{thread_id:prepared.thread_id.clone()}).map_err(|_|"The GPT view was closed before its conversation could be saved.")?;
        let saved=wait_for_saved(&prepared.thread_id,&mut controls,&mut cancel,&events,deadline).await;
        if let Err((status,error))=saved {
            connection.shutdown().await;
            return Ok(GptRunOutcome{thread_id:prepared.thread_id,status,error:Some(error)});
        }
        if prepared.transfer==Transfer::AlreadyAccepted {
            events.send(GptEvent::HandoffAccepted).map_err(|_|"The GPT view was closed.")?;
        }
        let mut state=RunState::new(prepared.thread_id.clone(),&request,events.clone());
        let result=execute(&mut connection,&mut state,&request,cwd.to_str().ok_or("The GPT workspace path is not valid UTF-8.")?,prepared.input,prepared.effort,prepared.transfer,&events,&mut controls,&mut cancel,deadline).await;
        state.resolve_all();
        connection.shutdown().await;
        match result {
            Ok((status,error))=>Ok(GptRunOutcome{thread_id:prepared.thread_id,status,error}),
            Err(error)=>Ok(GptRunOutcome{thread_id:prepared.thread_id,status:GptRunStatus::Failed,error:Some(error)}),
        }
    }
}

struct Prepared { thread_id:String, input:Vec<Value>, effort:Option<String>, transfer:Transfer }

async fn prepare(connection:&mut Connection, request:&GptRunRequest, cwd:&Path)->Result<Prepared,String> {
    require_account(connection).await?;
    let catalog=models(connection).await?;
    let selected=catalog.iter().find(|model|model.model==request.model)
        .ok_or("The selected GPT model is no longer in this account's catalog. Refresh models and select one.")?;
    let effort=selected.effort(request.thinking)?;
    let mut input=protocol::input(&request.action,selected.vision())?;
    let cwd=cwd.to_str().ok_or("The GPT workspace path is not valid UTF-8.")?;
    let method=if request.thread_id.is_some(){"thread/resume"}else{"thread/start"};
    let response=connection.call(method,protocol::thread_params(request,cwd)).await?;
    if response.get("model").and_then(Value::as_str)!=Some(request.model.as_str()) {
        return Err("Codex did not accept the selected GPT model. No work was started.".into());
    }
    if response.get("modelProvider").and_then(Value::as_str)!=Some("openai") {
        return Err("Codex selected a provider other than OpenAI. No work was started.".into());
    }
    if response.get("cwd").and_then(Value::as_str)!=Some(cwd) {
        return Err("Codex did not accept the selected working directory. No work was started.".into());
    }
    validate_effective_permissions(&response,request.mode,cwd)?;
    let mut thread=response.get("thread").cloned().ok_or("Codex did not return a conversation.")?;
    let thread_id=required_string(&thread,"id")?.to_owned();
    if thread_id.is_empty() || request.thread_id.as_deref().is_some_and(|id|id!=thread_id) {
        return Err("Codex returned an unexpected conversation ID. No work was started.".into());
    }
    if request.handoff.is_some() && request.thread_id.is_some() {
        if thread.get("historyMode").and_then(Value::as_str)==Some("paginated") || response.get("turnsBackwardsCursor").is_some_and(|cursor|!cursor.is_null()) {
            // Paginated threads deliberately omit old turns on resume. Read full
            // items in ascending order only until the first user input is found.
            thread["turns"]=Value::Array(first_user_turns(connection,&thread_id).await?);
        } else if thread.get("turns").and_then(Value::as_array).is_none() {
            let read=connection.call("thread/read",json!({"threadId":thread_id,"includeTurns":true})).await?;
            thread=read.get("thread").cloned().ok_or("Codex did not return the history needed to confirm task transfer delivery.")?;
            if required_string(&thread,"id")? != thread_id { return Err("Codex returned history for a different conversation.".into()); }
        }
    }
    let transfer=protocol::add_transfer(request,&thread,&mut input)?;
    Ok(Prepared{thread_id,input,effort,transfer})
}

fn validate_effective_permissions(response:&Value,mode:GptMode,cwd:&str)->Result<(),String> {
    let (sandbox,approval)=if mode==GptMode::Chat{("readOnly","never")}else{("workspaceWrite","untrusted")};
    if response.pointer("/sandbox/type").and_then(Value::as_str)!=Some(sandbox)
        || response.get("approvalPolicy").and_then(Value::as_str)!=Some(approval)
        || response.get("approvalsReviewer").and_then(Value::as_str)!=Some("user") {
        return Err("Codex did not accept Taceta's sandbox and user-approval policy. No work was started.".into());
    }
    if response.pointer("/sandbox/networkAccess").and_then(Value::as_bool)==Some(true) {
        return Err("Codex enabled tool network access unexpectedly. No work was started.".into());
    }
    if mode==GptMode::Coding {
        let policy=response.get("sandbox").ok_or("Codex did not return its effective sandbox.")?;
        if policy.get("excludeTmpdirEnvVar").and_then(Value::as_bool)!=Some(true) || policy.get("excludeSlashTmp").and_then(Value::as_bool)!=Some(true) {
            return Err("Codex did not apply Taceta's workspace write restrictions. No work was started.".into());
        }
        if policy.get("writableRoots").and_then(Value::as_array).is_some_and(|roots|roots.iter().any(|root|root.as_str()!=Some(cwd))) {
            return Err("Codex granted writes outside the selected workspace. No work was started.".into());
        }
    }
    Ok(())
}

async fn first_user_turns(connection:&mut Connection,thread_id:&str)->Result<Vec<Value>,String> {
    let mut turns=Vec::new();let mut cursor:Option<String>=None;let mut seen=HashSet::new();
    loop {
        let page=connection.call("thread/turns/list",json!({"threadId":thread_id,"sortDirection":"asc","itemsView":"full","cursor":cursor})).await?;
        let data=page.get("data").and_then(Value::as_array).ok_or("Codex did not return complete transfer history.")?;
        for turn in data {
            if turn.get("itemsView").and_then(Value::as_str).is_some_and(|view|view!="full") {return Err("Codex returned only a history summary; task transfer delivery cannot be established.".into());}
            let items=turn.get("items").and_then(Value::as_array).ok_or("Codex omitted items from transfer history.")?;
            let has_user=items.iter().any(|item|item.get("type").and_then(Value::as_str)==Some("userMessage"));
            turns.push(turn.clone());
            if has_user {return Ok(turns);}
        }
        cursor=match page.get("nextCursor") {None|Some(Value::Null)=>None,Some(Value::String(value)) if !value.is_empty()=>Some(value.clone()),_=>return Err("Codex returned an invalid history cursor.".into())};
        let Some(next)=&cursor else{return Ok(turns);};
        if !seen.insert(next.clone()){return Err("Codex repeated a history cursor; task transfer delivery cannot be established.".into());}
    }
}

fn validate_request(request:&GptRunRequest)->Result<(),String> {
    if request.model.trim().is_empty(){return Err("Select a GPT model first.".into());}
    if request.max_duration_secs==0 || request.max_tool_actions==0 { return Err("GPT time and tool-action limits must be greater than zero.".into()); }
    if request.thread_id.as_deref().is_some_and(|id|id.trim().is_empty()) { return Err("The saved GPT conversation ID is empty.".into()); }
    if matches!(request.action,GptAction::Compact) && request.thread_id.is_none(){return Err("There is no GPT conversation to compact.".into());}
    if matches!(request.action,GptAction::Continue) && request.thread_id.is_none() && request.handoff.is_none(){return Err("There is no GPT conversation or transferred task to continue.".into());}
    Ok(())
}

async fn wait_for_saved(thread_id:&str, controls:&mut UnboundedReceiver<GptControl>, cancel:&mut watch::Receiver<bool>, events:&UnboundedSender<GptEvent>, deadline:Instant)->Result<(),(GptRunStatus,String)> {
    loop {
        tokio::select! {
            biased;
            _=cancelled(cancel)=>return Err((GptRunStatus::Interrupted,"GPT was cancelled before its conversation was saved; no turn was started.".into())),
            _=events.closed()=>return Err((GptRunStatus::Interrupted,"The GPT view closed before its conversation was saved; no turn was started.".into())),
            _=sleep_until(deadline)=>return Err((GptRunStatus::LimitReached,"Taceta did not confirm that the GPT conversation ID was saved before the time limit; no turn was started.".into())),
            control=controls.recv()=>match control {
                Some(GptControl::ThreadSaved{thread_id:saved}) if saved==thread_id=>return Ok(()),
                None=>return Err((GptRunStatus::Interrupted,"The GPT control channel closed before saving the conversation; no turn was started.".into())),
                _=>{}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    connection:&mut Connection, state:&mut RunState, request:&GptRunRequest, cwd:&str,
    input:Vec<Value>,effort:Option<String>,transfer:Transfer,events:&UnboundedSender<GptEvent>,
    controls:&mut UnboundedReceiver<GptControl>,cancel:&mut watch::Receiver<bool>,deadline:Instant,
)->Result<(GptRunStatus,Option<String>),String> {
    let compact=matches!(request.action,GptAction::Compact);
    let method=if compact{"thread/compact/start"}else{"turn/start"};
    let params=if compact{json!({"threadId":state.thread_id})}else{protocol::turn_params(&state.thread_id,request,cwd,input,effort)};
    let start_id=timeout_at(deadline,connection.request(method,params)).await.map_err(|_|"The GPT time limit was reached while starting the turn; its effect is unknown.")??;
    let mut start_pending=true;
    let mut stop:Option<(GptRunStatus,String)>=None;
    let mut interrupt_id=None;
    let mut stop_deadline=None;
    loop {
        if stop.is_some() && stop_deadline.is_none(){stop_deadline=Some(Instant::now()+INTERRUPT_GRACE);}
        if stop.is_some() && interrupt_id.is_none() {
            if let Some(turn_id)=&state.turn_id {
                let sending=connection.request("turn/interrupt",json!({"threadId":state.thread_id,"turnId":turn_id}));
                interrupt_id=Some(timeout(INTERRUPT_GRACE,sending).await.map_err(|_|"Timed out while interrupting the GPT turn.")??);
            }
        }
        let wait_deadline=stop_deadline.unwrap_or(deadline);
        tokio::select! {
            biased;
            _=cancelled(cancel), if stop.is_none()=>stop=Some((GptRunStatus::Interrupted,"GPT was stopped by the user.".into())),
            _=events.closed(), if stop.is_none()=>stop=Some((GptRunStatus::Interrupted,"The GPT view was closed.".into())),
            _=sleep_until(wait_deadline)=>{
                if let Some((status,message))=stop {return Ok((status,Some(format!("{message} The owned Codex process was terminated after the stop grace period."))));}
                stop=Some((GptRunStatus::LimitReached,"The GPT time limit was reached.".into()));
            },
            control=controls.recv(), if stop.is_none()=>match control {
                Some(control)=>match state.control(control) {
                    Ok(Some((local_id,wire_id,response)))=>{
                        if let Err(error)=connection.respond(wire_id,response).await {stop=Some((GptRunStatus::Failed,error));}
                        let _=events.send(GptEvent::RequestResolved{id:local_id});
                    }
                    Ok(None)=>{},
                    Err(error)=>stop=Some((GptRunStatus::Failed,error)),
                },
                None=>stop=Some((GptRunStatus::Interrupted,"The GPT control channel was closed.".into())),
            },
            message=connection.next()=>{
                let message=match message {
                    Ok(message)=>message,
                    Err(error)=>return if let Some((status,message))=stop {Ok((status,Some(format!("{message} {error}"))))}else{Err(error)},
                };
                match message {
                    WireMessage::Response{id,result} if start_pending && id==start_id=>{
                        start_pending=false;
                        match result {
                            Ok(response)=>{
                                if !compact {
                                    state.set_turn(response.get("turn").ok_or("Codex turn/start response has no turn.")?)?;
                                    if transfer==Transfer::Included {let _=events.send(GptEvent::HandoffAccepted);}
                                }
                            }
                            Err(error)=>return if let Some((status,message))=stop{Ok((status,Some(format!("{message} {error}"))))}else{Err(error)},
                        }
                    }
                    WireMessage::Response{id,result} if interrupt_id.as_ref()==Some(&id)=>{
                        if let Err(error)=result {if let Some((_,message))=&mut stop{message.push_str(&format!(" Interrupt response: {error}"));}}
                    }
                    WireMessage::Response{..}=>{if stop.is_none(){stop=Some((GptRunStatus::Failed,"Codex returned an unknown response ID.".into()));}}
                    WireMessage::Request{id,method,params}=>{
                        if stop.is_some() {let _=connection.reject(id,"Taceta is stopping this turn.").await;}
                        else if let Err(error)=state.server_request(id.clone(),&method,&params) {
                            let _=connection.reject(id,&error).await;stop=Some((GptRunStatus::Failed,error));
                        }
                    }
                    WireMessage::Notification{method,params}=>match state.notification(&method,&params) {
                        Ok(Effect::Terminal(status,error))=>return if let Some((status,message))=stop{Ok((status,Some(message)))}else{Ok((status,error))},
                        Ok(Effect::ActionLimit) if stop.is_none()=>stop=Some((GptRunStatus::LimitReached,"The GPT tool-action stopping threshold was reached.".into())),
                        Ok(_)=>{},
                        Err(error)=>{if stop.is_none(){stop=Some((GptRunStatus::Failed,error));}}
                    }
                }
            }
        }
    }
}

async fn cancelled(cancel:&mut watch::Receiver<bool>) {
    loop {if *cancel.borrow_and_update(){return;} if cancel.changed().await.is_err(){return;}}
}

fn stopped_before_turn(request:&GptRunRequest,status:GptRunStatus,message:&str)->GptRunOutcome {
    GptRunOutcome{thread_id:request.thread_id.clone().unwrap_or_default(),status,error:Some(message.into())}
}

async fn open(chat:bool)->Result<Connection,String> {
    let paths=paths().await?;
    let mut connection=Connection::spawn(&paths.executable,&paths.home,&paths.chat,chat).await?;
    connection.initialize().await?;
    Ok(connection)
}

async fn paths()->Result<Paths,String> {
    let user_home=std::env::var_os("HOME").map(PathBuf::from).filter(|path|path.is_absolute())
        .ok_or("The macOS user home directory is unavailable.")?;
    let executable=find_executable(&user_home)?;
    let home=user_home.join("Library/Application Support/Taceta/gpt");
    private_directory(&home).await?;
    let chat=home.join("chat-workspace");
    private_directory(&chat).await?;
    Ok(Paths{home,chat,executable})
}

async fn private_directory(path:&Path)->Result<(),String> {
    use std::os::unix::fs::{MetadataExt,PermissionsExt};
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata)=>{
            if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid()!=unsafe{libc::geteuid()} {
                return Err("The Taceta GPT data directory must be a real directory owned by the current user.".into());
            }
        }
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>tokio::fs::create_dir_all(path).await.map_err(|error|format!("Could not create Taceta's GPT data directory: {error}"))?,
        Err(error)=>return Err(format!("Could not access Taceta's GPT data directory: {error}")),
    }
    tokio::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).await.map_err(|error|format!("Could not protect Taceta's GPT data directory: {error}"))
}

fn find_executable(user_home:&Path)->Result<PathBuf,String> {
    use std::os::unix::fs::PermissionsExt;
    let mut candidates=std::env::var_os("PATH").map(|path|std::env::split_paths(&path).filter(|path|path.is_absolute()).map(|path|path.join("codex")).collect::<Vec<_>>()).unwrap_or_default();
    candidates.extend([user_home.join(".local/bin/codex"),PathBuf::from("/opt/homebrew/bin/codex"),PathBuf::from("/usr/local/bin/codex")]);
    let mut seen=HashSet::new();
    for candidate in candidates {
        if seen.insert(candidate.clone()) && std::fs::metadata(&candidate).is_ok_and(|metadata|metadata.is_file() && metadata.permissions().mode()&0o111!=0){return Ok(candidate);}
    }
    Err("The official Codex CLI executable was not found on PATH or in the common macOS locations. Install Codex CLI, then connect GPT again; Taceta does not build or install it automatically.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ThinkingMode;
    use tokio::io::{AsyncBufReadExt,AsyncWriteExt,BufReader,duplex,split};
    use tokio::sync::mpsc;

    fn request()->GptRunRequest {GptRunRequest{thread_id:Some("thread".into()),workspace:None,mode:GptMode::Coding,model:"model".into(),thinking:ThinkingMode::Default,action:GptAction::Send{text:"work".into(),attachments:vec![]},handoff:None,max_duration_secs:30,max_tool_actions:5}}

    #[tokio::test]
    async fn gpt_thread_save_ack_matches_exact_thread_before_execution() {
        let (events,_rx)=mpsc::unbounded_channel();let (tx,mut controls)=mpsc::unbounded_channel();let (_cancel_tx,mut cancel)=watch::channel(false);
        tx.send(GptControl::ThreadSaved{thread_id:"other".into()}).unwrap();
        tx.send(GptControl::ThreadSaved{thread_id:"thread".into()}).unwrap();
        assert!(wait_for_saved("thread",&mut controls,&mut cancel,&events,Instant::now()+Duration::from_secs(1)).await.is_ok());
        drop(tx);
        assert!(wait_for_saved("thread",&mut controls,&mut cancel,&events,Instant::now()+Duration::from_secs(1)).await.is_err());
    }

    #[tokio::test]
    async fn gpt_cancel_interrupts_exact_turn_and_requires_terminal_event() {
        let (client,server)=duplex(8192);let (read,write)=split(client);let mut connection=Connection::from_io(read,write);
        let (events,_rx)=mpsc::unbounded_channel();let (_control_tx,mut controls)=mpsc::unbounded_channel();let (cancel_tx,mut cancel)=watch::channel(false);
        let req=request();let mut state=RunState::new("thread".into(),&req,events.clone());
        let peer=tokio::spawn(async move{
            let (read,mut write)=split(server);let mut read=BufReader::new(read);let mut line=String::new();
            read.read_line(&mut line).await.unwrap();let start:Value=serde_json::from_str(&line).unwrap();
            assert_eq!(start["method"],"turn/start");
            write.write_all(format!("{}\n",json!({"id":start["id"],"result":{"turn":{"id":"turn","status":"inProgress","items":[]}}})).as_bytes()).await.unwrap();
            cancel_tx.send(true).unwrap();line.clear();read.read_line(&mut line).await.unwrap();let interrupt:Value=serde_json::from_str(&line).unwrap();
            assert_eq!(interrupt["method"],"turn/interrupt");assert_eq!(interrupt["params"],json!({"threadId":"thread","turnId":"turn"}));
            write.write_all(format!("{}\n{}\n",json!({"id":interrupt["id"],"result":{}}),json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"interrupted","items":[]}}})).as_bytes()).await.unwrap();
        });
        let result=execute(&mut connection,&mut state,&req,"/workspace",vec![json!({"type":"text","text":"work"})],None,Transfer::None,&events,&mut controls,&mut cancel,Instant::now()+Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.0,GptRunStatus::Interrupted);peer.await.unwrap();
    }

    #[tokio::test]
    async fn gpt_eof_after_start_is_failure_not_completed() {
        let (client,server)=duplex(8192);let (read,write)=split(client);let mut connection=Connection::from_io(read,write);
        let (events,_rx)=mpsc::unbounded_channel();let (_control_tx,mut controls)=mpsc::unbounded_channel();let (_cancel_tx,mut cancel)=watch::channel(false);
        let req=request();let mut state=RunState::new("thread".into(),&req,events.clone());
        let peer=tokio::spawn(async move{
            let (read,mut write)=split(server);let mut line=String::new();BufReader::new(read).read_line(&mut line).await.unwrap();let start:Value=serde_json::from_str(&line).unwrap();
            write.write_all(format!("{}\n",json!({"id":start["id"],"result":{"turn":{"id":"turn","status":"inProgress","items":[]}}})).as_bytes()).await.unwrap();
        });
        assert!(execute(&mut connection,&mut state,&req,"/workspace",vec![],None,Transfer::None,&events,&mut controls,&mut cancel,Instant::now()+Duration::from_secs(5)).await.is_err());
        peer.await.unwrap();
    }

    #[test]
    fn gpt_effective_permissions_fail_closed_before_a_turn_can_run() {
        let mut response=json!({"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite","writableRoots":["/workspace"],"networkAccess":false,"excludeSlashTmp":true,"excludeTmpdirEnvVar":true}});
        assert!(validate_effective_permissions(&response,GptMode::Coding,"/workspace").is_ok());
        response["sandbox"]["networkAccess"]=json!(true);
        assert!(validate_effective_permissions(&response,GptMode::Coding,"/workspace").is_err());
        response["sandbox"]["networkAccess"]=json!(false);response["approvalsReviewer"]=json!("auto_review");
        assert!(validate_effective_permissions(&response,GptMode::Coding,"/workspace").is_err());
    }
}
