//! Model-independent coding execution, durable history and bounded working context.
//! Inference adapters never execute tools; this module owns every side effect.

mod context;
mod journal;
mod state;
mod types;
mod workspace;

pub use context::reserved_output_tokens;
pub use types::*;

use context::ContextPlan;
use journal::{EventData, Journal};
use serde::Deserialize;
use state::State;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        watch,
    },
    time::Instant,
};
use uuid::Uuid;
use workspace::{PreparedTool, WorkspaceTools};

const MAX_CONSECUTIVE_TOOL_FAILURES: u32 = 3;
const INFERENCE_TIMEOUT_SECS: u64 = 300;
const MAX_HISTORY_PAGE_BYTES: usize = 16_384;

pub struct AgentSession {
    root: PathBuf,
    journal: Journal,
    state: State,
}

impl AgentSession {
    pub fn create(root: &Path, id: Uuid, workspace: &Path) -> Result<Self, String> {
        let workspace = WorkspaceTools::new(workspace)?;
        separate_storage(root, workspace.workspace())?;
        let journal = Journal::create(root, id, workspace.workspace())?;
        let root = journal
            .directory
            .parent()
            .ok_or("Missing session root")?
            .to_owned();
        let state = State::restore(&journal.records)?;
        Ok(Self {
            root,
            journal,
            state,
        })
    }

    pub fn open(root: &Path, id: Uuid) -> Result<Self, String> {
        let journal = Journal::open(root, id)?;
        let root = journal
            .directory
            .parent()
            .ok_or("Missing session root")?
            .to_owned();
        let mut state = State::restore(&journal.records)?;
        // Reading a crashed session exposes interruption without replaying it.
        // An active owner's lock preserves its live Running/AwaitingApproval view.
        if matches!(
            state.snapshot.status,
            SessionStatus::Running | SessionStatus::AwaitingApproval
        ) && journal.lock().is_ok()
        {
            state.snapshot.status = SessionStatus::Interrupted;
            state.snapshot.error = Some(
                "Previous run ended without a durable terminal status; no effects were replayed"
                    .into(),
            );
        }
        Ok(Self {
            root,
            journal,
            state,
        })
    }

    pub fn open_or_create(root: &Path, id: Uuid, workspace: &Path) -> Result<(Self, bool), String> {
        let selected = WorkspaceTools::new(workspace)?;
        let path = root.join(id.to_string());
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let session = Self::open(root, id)?;
                if session.state.snapshot.workspace != selected.workspace() {
                    return Err("This conversation already belongs to a different workspace; select another conversation".into());
                }
                Ok((session, false))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Self::create(root, id, selected.workspace()).map(|session| (session, true))
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn list_ids(root: &Path) -> Result<Vec<Uuid>, String> {
        if !root.exists() {
            return Ok(Vec::new());
        }
        journal::private_directory(root)?;
        let mut ids = Vec::new();
        for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if let Some(name) = entry.file_name().to_str()
                && let Ok(id) = Uuid::parse_str(name)
            {
                journal::private_directory(&entry.path())?;
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Moves one inactive session to private recovery storage. No raw event is deleted.
    pub fn archive(root: &Path, id: Uuid) -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        match fs::symlink_metadata(root.join(id.to_string())) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
            Ok(_) => {}
        }
        let journal = Journal::open(root, id)?;
        let _lock = journal.lock()?;
        let archive = journal
            .directory
            .parent()
            .ok_or("Missing session root")?
            .join("archive");
        if !archive.exists() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&archive)
                .map_err(|e| e.to_string())?;
        }
        journal::private_directory(&archive)?;
        let destination = archive.join(format!("{id}-{}", Uuid::new_v4()));
        fs::rename(&journal.directory, &destination).map_err(|e| e.to_string())?;
        fs::File::open(&archive)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Imports existing plain chat exactly once, before any agent run or input.
    /// The caller excludes Thinking and interrupted assistant output.
    pub fn import_messages(&mut self, messages: Vec<AgentMessage>) -> Result<(), String> {
        let _lock = self.journal.lock()?;
        self.reload()?;
        if self.journal.records.len() != 1 {
            return Err(
                "Chat import is allowed only into a newly created empty agent session".into(),
            );
        }
        if messages.iter().any(|message| {
            !matches!(message.role, AgentRole::User | AgentRole::Assistant)
                || !message.tool_calls.is_empty()
                || message.tool_call_id.is_some()
                || message.tool_name.is_some()
        }) {
            return Err(
                "Chat import accepts only plain User and Assistant messages without tool data"
                    .into(),
            );
        }
        self.record(EventData::ImportedMessages { messages })?;
        Ok(())
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.state.snapshot.clone()
    }

    fn reload(&mut self) -> Result<(), String> {
        self.journal = Journal::open(&self.root, self.state.snapshot.id)?;
        self.state = State::restore(&self.journal.records)?;
        Ok(())
    }

    fn record(&mut self, event: EventData) -> Result<(), String> {
        self.journal.append(event)?;
        self.state.apply(
            self.journal
                .records
                .last()
                .ok_or("Missing appended event")?,
        )
    }

    fn send_snapshot(&self, events: &UnboundedSender<AgentEvent>) {
        let _ = events.send(AgentEvent::Snapshot(self.snapshot()));
    }

    fn close_unresolved(&mut self, reason: &str) -> Result<(), String> {
        for (call, started) in self.state.unresolved() {
            let disposition = if started {
                "Outcome unknown: execution started but no durable result exists. The effect may have occurred. Inspect current workspace state before a new proposal; never automatically repeat it."
            } else {
                "Not executed: this tool call did not reach durable execution start. Any prior approval expired; a new proposal needs fresh approval."
            };
            self.record(EventData::ToolResult {
                call_id: call.id,
                name: call.name,
                output: format!("{disposition} {reason}"),
                is_error: true,
            })?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        status: SessionStatus,
        message: Option<String>,
        events: &UnboundedSender<AgentEvent>,
    ) -> Result<SessionSnapshot, String> {
        self.close_unresolved(message.as_deref().unwrap_or("Run ended"))?;
        self.record(EventData::StatusChanged { status, message })?;
        self.send_snapshot(events);
        Ok(self.snapshot())
    }

    pub async fn run(
        mut self,
        config: RunConfig,
        model: Arc<dyn AgentModel>,
        events: UnboundedSender<AgentEvent>,
        mut approvals: UnboundedReceiver<ApprovalDecision>,
        cancel: watch::Receiver<bool>,
    ) -> Result<SessionSnapshot, String> {
        if config.model.trim().is_empty()
            || config.max_steps == 0
            || config.max_steps > 10_000
            || config.max_duration_secs == 0
            || config.max_duration_secs > 86_400
        {
            return Err(
                "Agent run needs a model, 1..10000 inference calls and 1..86400 seconds".into(),
            );
        }
        context::input_budget(config.context_length)?;
        if config.compact_only && config.prompt.is_some() {
            return Err(
                "Manual compaction accepts only a saved session, without a new user prompt".into(),
            );
        }
        if config
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            return Err("An added user instruction cannot be empty".into());
        }
        let _lock = self.journal.lock()?;
        self.reload()?;
        self.journal.preserve_partials()?;
        self.state = State::restore(&self.journal.records)?;
        self.close_unresolved("Recovered after interruption or process exit.")?;
        let workspace = WorkspaceTools::new(&self.state.snapshot.workspace)?;
        separate_storage(&self.root, workspace.workspace())?;
        if workspace.workspace() != self.state.snapshot.workspace {
            return Err("The stored workspace identity changed; no execution occurred".into());
        }
        let workspace_instructions = workspace.instructions()?;
        if let Some(prompt) = &config.prompt {
            self.record(EventData::UserInput {
                content: prompt.clone(),
            })?;
        }
        if self.state.instructions.is_empty() {
            return Err("The session has no user task to execute".into());
        }
        if !config.compact_only
            && config.prompt.is_none()
            && self.state.snapshot.status == SessionStatus::Completed
        {
            self.send_snapshot(&events);
            return Ok(self.snapshot());
        }
        let previous_status = self.state.snapshot.status.clone();
        self.record(EventData::RunStarted {
            model: config.model.clone(),
            context_length: config.context_length,
            max_steps: config.max_steps,
            max_duration_secs: config.max_duration_secs,
            compact_only: config.compact_only,
        })?;
        self.send_snapshot(&events);
        let deadline = Instant::now() + Duration::from_secs(config.max_duration_secs);
        let definitions = tool_definitions();
        let mut calls = 0;
        let mut failures = 0;
        let mut seen_call_ids = self.journal.seen_call_ids();
        loop {
            if *cancel.borrow() {
                return self.finish(
                    SessionStatus::Interrupted,
                    Some("Stopped by the user".into()),
                    &events,
                );
            }
            if Instant::now() >= deadline {
                return self.finish(
                    SessionStatus::LimitReached,
                    Some("The total run time limit was reached".into()),
                    &events,
                );
            }
            let plan = match context::assemble(
                &self.state,
                &workspace_instructions,
                &definitions,
                config.context_length,
                &config.model,
                config.compact_only && calls == 0,
            ) {
                Ok(plan) => plan,
                Err(error) => return self.finish(SessionStatus::Failed, Some(error), &events),
            };
            let (messages, tools, compaction) = match plan {
                ContextPlan::Ready(messages) => {
                    if config.compact_only {
                        let _ = events.send(AgentEvent::Progress(
                            "履歴の整理が終わりました。再開を待っています。".into(),
                        ));
                        let status = if previous_status == SessionStatus::Completed {
                            SessionStatus::Completed
                        } else {
                            SessionStatus::Interrupted
                        };
                        return self.finish(status, None, &events);
                    }
                    (messages, definitions.clone(), None)
                }
                ContextPlan::Compact(plan) => {
                    let _ = events.send(AgentEvent::Progress(format!(
                        "履歴の原文を保持して圧縮中（イベント {}〜{}）",
                        plan.from_seq, plan.to_seq
                    )));
                    (plan.messages.clone(), Vec::new(), Some(plan))
                }
            };
            if calls >= config.max_steps {
                return self.finish(
                    SessionStatus::LimitReached,
                    Some("The inference-call limit was reached, including compaction calls".into()),
                    &events,
                );
            }
            calls += 1;
            self.record(EventData::InferenceStarted {
                model: config.model.clone(),
                compaction: compaction.is_some(),
                step: calls,
            })?;
            let inference = infer(
                model.as_ref(),
                AgentRequest {
                    model: config.model.clone(),
                    messages,
                    tools,
                    thinking: config.thinking,
                    context_length: config.context_length,
                },
                &events,
                cancel.clone(),
                deadline,
                compaction.is_some(),
            )
            .await;
            let turn = match inference {
                Ok(turn) => turn,
                Err(stop) => {
                    if !stop.partial.is_empty() {
                        self.record(EventData::InterruptedOutput {
                            content: stop.partial,
                        })?;
                    }
                    return self.finish(stop.status, Some(stop.message), &events);
                }
            };
            if let Some(plan) = compaction {
                if !turn.tool_calls.is_empty()
                    || turn.content.trim().is_empty()
                    || turn.content.len() > plan.maximum_summary_bytes
                {
                    return self.finish(SessionStatus::Failed,
                        Some(format!("Compaction did not return a plain non-empty summary within {} UTF-8 bytes; all original events were retained", plan.maximum_summary_bytes)), &events);
                }
                self.record(EventData::Compacted {
                    summary: CompactionSummary {
                        from_seq: plan.from_seq,
                        to_seq: plan.to_seq,
                        through_event_id: format!("{}:{}", self.state.snapshot.id, plan.to_seq),
                        through_message_index: plan.through_message_index,
                        window_id: Uuid::new_v4(),
                        previous_window_id: self
                            .state
                            .snapshot
                            .summary
                            .as_ref()
                            .map(|summary| summary.window_id),
                        source_last_seq: plan.source_last_seq,
                        retained_ranges: plan.retained_ranges,
                        model: config.model.clone(),
                        context_length: config.context_length,
                        content: turn.content,
                    },
                })?;
                self.send_snapshot(&events);
                continue;
            }
            if let Err(error) = validate_turn(&turn, &mut seen_call_ids) {
                return self.finish(SessionStatus::Failed, Some(error), &events);
            }
            self.record(EventData::AssistantTurn { turn: turn.clone() })?;
            self.send_snapshot(&events);
            if turn.tool_calls.is_empty() {
                return self.finish(SessionStatus::Completed, None, &events);
            }
            for call in turn.tool_calls {
                if *cancel.borrow() {
                    return self.finish(
                        SessionStatus::Interrupted,
                        Some("Stopped by the user".into()),
                        &events,
                    );
                }
                if Instant::now() >= deadline {
                    return self.finish(
                        SessionStatus::LimitReached,
                        Some("The total run time limit was reached before tool execution".into()),
                        &events,
                    );
                }
                let prepared = match self.prepare(&workspace, &call) {
                    Ok(tool) => tool,
                    Err(error) => {
                        self.tool_result(&call, Err(error))?;
                        failures += 1;
                        if failures >= MAX_CONSECUTIVE_TOOL_FAILURES {
                            return self.finish(SessionStatus::Failed, Some("Three consecutive tool failures; no further effects were executed".into()), &events);
                        }
                        continue;
                    }
                };
                if let Some((description, details)) = prepared.proposal() {
                    let request = ApprovalRequest {
                        id: Uuid::new_v4(),
                        tool_call_id: call.id.clone(),
                        description,
                        details,
                    };
                    self.record(EventData::ApprovalRequested {
                        request: request.clone(),
                    })?;
                    self.send_snapshot(&events);
                    if events
                        .send(AgentEvent::ApprovalRequired(request.clone()))
                        .is_err()
                    {
                        return self.finish(
                            SessionStatus::Interrupted,
                            Some(
                                "Approval UI disconnected; the proposed effect was not executed"
                                    .into(),
                            ),
                            &events,
                        );
                    }
                    match wait_approval(request.id, &mut approvals, cancel.clone(), deadline).await
                    {
                        Ok(approved) => {
                            self.record(EventData::ApprovalResolved {
                                id: request.id,
                                approved,
                            })?;
                            if !approved {
                                self.tool_result(&call, Err("User denied this exact proposal. No effect was executed; this is not permission to re-propose the same effect automatically.".into()))?;
                                failures += 1;
                                if failures >= MAX_CONSECUTIVE_TOOL_FAILURES {
                                    return self.finish(
                                        SessionStatus::Failed,
                                        Some(
                                            "Three consecutive denied or failed tools; stopped"
                                                .into(),
                                        ),
                                        &events,
                                    );
                                }
                                self.send_snapshot(&events);
                                continue;
                            }
                        }
                        Err(stop) => return self.finish(stop.status, Some(stop.message), &events),
                    }
                }
                // Every effect is durably marked before starting. Recovery never
                // converts this marker or an approval into automatic re-execution.
                self.record(EventData::ToolStarted { call: call.clone() })?;
                let _ = events.send(AgentEvent::Progress(format!("{} を実行中", call.name)));
                let result = match prepared {
                    Prepared::Workspace(tool) => {
                        tokio::select! {
                            biased;
                            _ = cancellation(cancel.clone()) => return self.finish(SessionStatus::Interrupted, Some("Stopped during tool execution; inspect its recorded outcome before resuming".into()), &events),
                            _ = tokio::time::sleep_until(deadline) => return self.finish(SessionStatus::LimitReached, Some("The total run time limit was reached during tool execution".into()), &events),
                            result = tool.execute(cancel.clone()) => result,
                        }
                    }
                    Prepared::WorkState(state) => {
                        self.record(EventData::WorkStateUpdated { state })?;
                        Ok("Structured work state saved. Original user instructions remain unchanged.".into())
                    }
                    Prepared::HistorySearch(arguments) => self.search_history(arguments),
                    Prepared::HistoryRead(arguments) => self.read_history(arguments),
                };
                if result.is_err() {
                    failures += 1;
                } else {
                    failures = 0;
                }
                self.tool_result(&call, result)?;
                self.send_snapshot(&events);
                if failures >= MAX_CONSECUTIVE_TOOL_FAILURES {
                    return self.finish(SessionStatus::Failed, Some("Three consecutive tool failures; stopped with original history preserved".into()), &events);
                }
            }
        }
    }

    fn tool_result(
        &mut self,
        call: &AgentToolCall,
        result: Result<String, String>,
    ) -> Result<(), String> {
        let (output, is_error) = match result {
            Ok(output) => (output, false),
            Err(error) => (error, true),
        };
        self.record(EventData::ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output,
            is_error,
        })
    }

    fn prepare(
        &self,
        workspace: &WorkspaceTools,
        call: &AgentToolCall,
    ) -> Result<Prepared, String> {
        match call.name.as_str() {
            "update_work_state" => {
                let state: WorkState = serde_json::from_value(call.arguments.clone())
                    .map_err(|e| format!("Invalid work state: {e}"))?;
                let size = serde_json::to_vec(&state).map_err(|e| e.to_string())?.len();
                let mut ids = BTreeSet::new();
                if size > 65_536
                    || state.tasks.len() > 100
                    || state.tasks.iter().any(|task| {
                        task.id.trim().is_empty()
                            || task.description.trim().is_empty()
                            || !ids.insert(task.id.clone())
                    })
                {
                    return Err(
                        "Work state exceeds its limit or has empty/duplicate task IDs".into(),
                    );
                }
                Ok(Prepared::WorkState(state))
            }
            "search_history" => {
                let arguments: HistorySearch =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                if arguments.query.is_empty()
                    || arguments.query.len() > 1024
                    || !(1..=50).contains(&arguments.limit)
                {
                    return Err(
                        "History search needs a non-empty query up to 1024 bytes and limit 1..50"
                            .into(),
                    );
                }
                Ok(Prepared::HistorySearch(arguments))
            }
            "read_history" => {
                let arguments: HistoryRead =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                if arguments.seq == 0
                    || arguments.max_bytes == 0
                    || arguments.max_bytes > MAX_HISTORY_PAGE_BYTES
                {
                    return Err(
                        "History read needs an event sequence and max_bytes 1..16384".into(),
                    );
                }
                Ok(Prepared::HistoryRead(arguments))
            }
            _ => workspace.prepare(call).map(Prepared::Workspace),
        }
    }

    fn search_history(&self, arguments: HistorySearch) -> Result<String, String> {
        let mut matches = Vec::new();
        let mut next_seq = None;
        for record in self
            .journal
            .records
            .iter()
            .filter(|record| record.seq >= arguments.start_seq)
        {
            let raw = serde_json::to_string(record).map_err(|e| e.to_string())?;
            if let Some(position) = raw.find(&arguments.query) {
                if matches.len() == arguments.limit {
                    next_seq = Some(record.seq);
                    break;
                }
                let mut start = position.saturating_sub(120);
                while !raw.is_char_boundary(start) {
                    start -= 1;
                }
                let mut end = (position + arguments.query.len() + 240).min(raw.len());
                while !raw.is_char_boundary(end) {
                    end -= 1;
                }
                matches.push(serde_json::json!({"seq": record.seq, "excerpt": &raw[start..end], "match_byte_offset": position}));
            }
        }
        Ok(serde_json::json!({"untrusted_history": true, "matches": matches, "next_seq": next_seq}).to_string())
    }

    fn read_history(&self, arguments: HistoryRead) -> Result<String, String> {
        let record = self
            .journal
            .records
            .get((arguments.seq - 1) as usize)
            .ok_or("No such history event")?;
        let raw = serde_json::to_string(record).map_err(|e| e.to_string())?;
        if arguments.offset > raw.len() || !raw.is_char_boundary(arguments.offset) {
            return Err("History offset must be a UTF-8 boundary inside the original event".into());
        }
        let mut end = arguments
            .offset
            .saturating_add(arguments.max_bytes)
            .min(raw.len());
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        if end == arguments.offset && end < raw.len() {
            return Err("max_bytes cannot hold the next UTF-8 character".into());
        }
        Ok(
            serde_json::json!({"untrusted_history": true, "seq": arguments.seq,
            "offset": arguments.offset, "total_bytes": raw.len(),
            "next_offset": (end < raw.len()).then_some(end), "raw": &raw[arguments.offset..end]})
            .to_string(),
        )
    }
}

fn separate_storage(root: &Path, workspace: &Path) -> Result<(), String> {
    use std::path::Component;
    if root
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("Session storage path cannot contain parent-directory components".into());
    }
    let mut existing = if root.is_absolute() {
        root.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(root)
    };
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or("Cannot resolve session storage root")?
                .to_owned(),
        );
        if !existing.pop() {
            return Err("Cannot resolve session storage root".into());
        }
    }
    let mut resolved = fs::canonicalize(existing).map_err(|e| e.to_string())?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    if resolved.starts_with(workspace) || workspace.starts_with(&resolved) {
        return Err("Workspace and private session storage must not overlap; workspace tools cannot modify their own journal or approvals".into());
    }
    Ok(())
}

fn validate_turn(turn: &AgentTurn, seen: &mut BTreeSet<String>) -> Result<(), String> {
    if turn.content.trim().is_empty() && turn.tool_calls.is_empty() {
        return Err("Model returned neither an answer nor a tool call".into());
    }
    if turn.tool_calls.len() > 8 || turn.content.len() > 1_048_576 {
        return Err("Model turn exceeds the bounded content/tool-call limit".into());
    }
    let mut fresh = BTreeSet::new();
    for call in &turn.tool_calls {
        if call.id.is_empty()
            || call.id.len() > 256
            || call.name.is_empty()
            || call.name.len() > 128
            || call.id.chars().any(char::is_control)
            || !call.arguments.is_object()
            || serde_json::to_vec(&call.arguments)
                .map_err(|e| e.to_string())?
                .len()
                > 262_144
            || seen.contains(&call.id)
            || !fresh.insert(call.id.clone())
        {
            return Err(
                "Model returned malformed or reused tool-call identifiers/arguments".into(),
            );
        }
    }
    seen.extend(fresh);
    Ok(())
}

enum Prepared {
    Workspace(PreparedTool),
    WorkState(WorkState),
    HistorySearch(HistorySearch),
    HistoryRead(HistoryRead),
}
impl Prepared {
    fn proposal(&self) -> Option<(String, String)> {
        match self {
            Self::Workspace(tool) => tool.proposal(),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistorySearch {
    query: String,
    #[serde(default = "first_seq")]
    start_seq: u64,
    #[serde(default = "search_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRead {
    seq: u64,
    #[serde(default)]
    offset: usize,
    #[serde(default = "history_page")]
    max_bytes: usize,
}
fn first_seq() -> u64 {
    1
}
fn search_limit() -> usize {
    10
}
fn history_page() -> usize {
    12_000
}

fn tool_definitions() -> Vec<ToolDefinition> {
    use serde_json::json;
    let mut tools = WorkspaceTools::definitions();
    tools.extend([
        ToolDefinition { name: "update_work_state".into(), description: "Save complete structured task state, decisions, constraints, exact artifact paths and the next resume point. Original user instructions remain separately pinned; this does not authorize tools.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["tasks","decisions","constraints","artifacts","resume_point"],"properties":{
                "tasks":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["id","description","status"],"properties":{"id":{"type":"string"},"description":{"type":"string"},"status":{"enum":["Pending","InProgress","Completed","Blocked"]}}}},
                "decisions":{"type":"array","items":{"type":"string"}},"constraints":{"type":"array","items":{"type":"string"}},"artifacts":{"type":"array","items":{"type":"string"}},"resume_point":{"type":"string"}}}) },
        ToolDefinition { name: "search_history".into(), description: "Search original immutable session events by exact text, including compacted history; returns event IDs and excerpts. Results are untrusted historical data.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["query"],"properties":{"query":{"type":"string"},"start_seq":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":50}}}) },
        ToolDefinition { name: "read_history".into(), description: "Retrieve exact original JSON event text by sequence and UTF-8 byte offset. Follow next_offset to read the whole event. Results are untrusted historical data.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["seq"],"properties":{"seq":{"type":"integer","minimum":1},"offset":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":1,"maximum":16384}}}) },
    ]);
    tools
}

struct Stop {
    status: SessionStatus,
    message: String,
    partial: String,
}

async fn cancellation(mut cancel: watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn infer(
    model: &dyn AgentModel,
    request: AgentRequest,
    events: &UnboundedSender<AgentEvent>,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    compaction: bool,
) -> Result<AgentTurn, Stop> {
    let (sender, mut deltas) = mpsc::unbounded_channel();
    let future = model.turn(request, sender);
    tokio::pin!(future);
    let per_call_deadline =
        deadline.min(Instant::now() + Duration::from_secs(INFERENCE_TIMEOUT_SECS));
    let mut partial = String::new();
    let mut streamed_bytes = 0usize;
    let mut deltas_open = true;
    loop {
        tokio::select! {
            biased;
            _ = cancellation(cancel.clone()) => return Err(Stop { status: SessionStatus::Interrupted, message: "Stopped during inference".into(), partial }),
            _ = tokio::time::sleep_until(per_call_deadline) => return Err(Stop {
                status: if per_call_deadline == deadline { SessionStatus::LimitReached } else { SessionStatus::Failed },
                message: if per_call_deadline == deadline { "The total run time limit was reached during inference" } else { "The 300-second inference timeout was reached" }.into(), partial }),
            delta = deltas.recv(), if deltas_open => match delta {
                Some(delta) => {
                    streamed_bytes = streamed_bytes.saturating_add(match &delta {
                        ModelDelta::Content(text) | ModelDelta::Thinking(text) => text.len(),
                    });
                    if streamed_bytes > 2_097_152 {
                        return Err(Stop { status: SessionStatus::Failed, message: "Streaming inference exceeded its bounded output limit".into(), partial });
                    }
                    if !compaction {
                        if let ModelDelta::Content(text) = &delta {
                            if partial.len().saturating_add(text.len()) > 1_048_576 {
                                return Err(Stop { status: SessionStatus::Failed, message: "Streaming response exceeded its bounded output limit".into(), partial });
                            }
                            partial.push_str(text);
                        }
                        let _ = events.send(AgentEvent::Delta(delta));
                    }
                }
                None => deltas_open = false,
            },
            result = &mut future => return result.map_err(|error| Stop { status: SessionStatus::Failed,
                message: if compaction { format!("Compaction failed; all original history was retained: {error}") } else { error }, partial }),
        }
    }
}

async fn wait_approval(
    id: Uuid,
    approvals: &mut UnboundedReceiver<ApprovalDecision>,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
) -> Result<bool, Stop> {
    loop {
        tokio::select! {
            biased;
            _ = cancellation(cancel.clone()) => return Err(Stop { status: SessionStatus::Interrupted, message: "Stopped while waiting for approval; the proposal was not executed".into(), partial: String::new() }),
            _ = tokio::time::sleep_until(deadline) => return Err(Stop { status: SessionStatus::LimitReached, message: "Run time expired while waiting for approval; the proposal was not executed".into(), partial: String::new() }),
            decision = approvals.recv() => match decision {
                Some(decision) if decision.id == id => return Ok(decision.approved),
                Some(_) => {}, // stale/foreign decisions can never authorize this proposal
                None => return Err(Stop { status: SessionStatus::Interrupted, message: "Approval channel closed; the proposal was not executed".into(), partial: String::new() }),
            }
        }
    }
}

#[cfg(test)]
mod tests;
