use super::*;
use std::collections::HashMap;
use taceta::agent::{
    AgentEvent, AgentMessage, AgentModel, AgentRole, AgentSession, ApprovalDecision,
    ApprovalRequest, ModelDelta, RunConfig, SessionSnapshot, SessionStatus,
};
use tokio::sync::watch;

pub(super) struct ActiveAgent {
    task: JoinHandle<()>,
    conversation_id: Uuid,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    result: std_mpsc::Receiver<Result<SessionSnapshot, String>>,
    cancel: watch::Sender<bool>,
    approvals: mpsc::UnboundedSender<ApprovalDecision>,
    approval: Option<PendingApproval>,
    content: String,
    thinking: String,
    progress: String,
    submitted_prompt: Option<String>,
    /// Messages from other engines that this run imports into the task
    /// record. Once the run has a result they are part of that record.
    imported_external: Vec<Uuid>,
}

/// An approval request with its details laid out for reading.
struct PendingApproval {
    request: ApprovalRequest,
    view: ApprovalView,
    /// Decisions are ignored briefly after a request appears, so a click
    /// meant for the previous request cannot answer this one.
    shown_at: std::time::Instant,
}

const APPROVAL_SETTLE: Duration = Duration::from_millis(600);

#[derive(Debug, PartialEq, Eq)]
enum ApprovalView {
    Edit { path: String, created: bool, lines: Vec<DiffLine> },
    Fields(Vec<(String, String)>),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DiffLine {
    Same(String),
    Removed(String),
    Added(String),
    /// Unchanged lines left out between two changed regions.
    Skipped(usize),
}

impl ApprovalView {
    /// Proposal details arrive as JSON written by the workspace tools. Edits
    /// become a line diff, anything else its fields, and unknown shapes are
    /// shown exactly as received.
    fn from_details(details: &str) -> Self {
        let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(details) else {
            return Self::Text(details.to_owned());
        };
        if let (Some(path), Some(after)) = (
            fields.get("path").and_then(|value| value.as_str()),
            fields.get("after").and_then(|value| value.as_str()),
        ) {
            let before = fields.get("before").and_then(|value| value.as_str());
            return Self::Edit {
                path: path.to_owned(),
                created: before.is_none(),
                lines: line_diff(before.unwrap_or_default(), after),
            };
        }
        Self::Fields(
            fields
                .into_iter()
                .map(|(name, value)| {
                    let value = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
                    (name, value)
                })
                .collect(),
        )
    }
}

/// Lines kept around each change so its place in the file is recognisable.
const DIFF_CONTEXT: usize = 3;
/// Above this many differing lines per side the middle is shown as one
/// removed block and one added block instead of being aligned line by line.
const DIFF_ALIGN_LIMIT: usize = 2_000;

fn line_diff(before: &str, after: &str) -> Vec<DiffLine> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_mid, new_mid) = (&old[prefix..old.len() - suffix], &new[prefix..new.len() - suffix]);

    let mut all: Vec<DiffLine> = old[..prefix].iter().map(|line| DiffLine::Same((*line).to_owned())).collect();
    if old_mid.len() > DIFF_ALIGN_LIMIT || new_mid.len() > DIFF_ALIGN_LIMIT {
        all.extend(old_mid.iter().map(|line| DiffLine::Removed((*line).to_owned())));
        all.extend(new_mid.iter().map(|line| DiffLine::Added((*line).to_owned())));
    } else {
        // Longest common subsequence over the differing middle.
        let (rows, columns) = (old_mid.len(), new_mid.len());
        let mut common = vec![0u32; (rows + 1) * (columns + 1)];
        for row in (0..rows).rev() {
            for column in (0..columns).rev() {
                common[row * (columns + 1) + column] = if old_mid[row] == new_mid[column] {
                    common[(row + 1) * (columns + 1) + column + 1] + 1
                } else {
                    common[(row + 1) * (columns + 1) + column].max(common[row * (columns + 1) + column + 1])
                };
            }
        }
        let (mut row, mut column) = (0, 0);
        while row < rows && column < columns {
            if old_mid[row] == new_mid[column] {
                all.push(DiffLine::Same(old_mid[row].to_owned()));
                row += 1;
                column += 1;
            } else if common[(row + 1) * (columns + 1) + column] >= common[row * (columns + 1) + column + 1] {
                all.push(DiffLine::Removed(old_mid[row].to_owned()));
                row += 1;
            } else {
                all.push(DiffLine::Added(new_mid[column].to_owned()));
                column += 1;
            }
        }
        all.extend(old_mid[row..].iter().map(|line| DiffLine::Removed((*line).to_owned())));
        all.extend(new_mid[column..].iter().map(|line| DiffLine::Added((*line).to_owned())));
    }
    all.extend(old[old.len() - suffix..].iter().map(|line| DiffLine::Same((*line).to_owned())));

    // Keep changed lines and their surroundings; fold the rest.
    let changed: Vec<bool> = all.iter().map(|line| !matches!(line, DiffLine::Same(_))).collect();
    let keep: Vec<bool> = (0..all.len())
        .map(|index| {
            let from = index.saturating_sub(DIFF_CONTEXT);
            let to = (index + DIFF_CONTEXT + 1).min(all.len());
            changed[from..to].iter().any(|changed| *changed)
        })
        .collect();
    let mut shown = Vec::new();
    let mut skipped = 0;
    for (line, keep) in all.into_iter().zip(keep) {
        if keep {
            if skipped > 0 {
                shown.push(DiffLine::Skipped(skipped));
                skipped = 0;
            }
            shown.push(line);
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        shown.push(DiffLine::Skipped(skipped));
    }
    shown
}

pub(super) struct AgentUiState {
    pub active: Option<ActiveAgent>,
    snapshots: HashMap<Uuid, SessionSnapshot>,
    pub(super) data_root: PathBuf,
    recovery_started: bool,
    recovery: Option<std_mpsc::Receiver<Vec<Result<SessionSnapshot, String>>>>,
}

impl AgentUiState {
    /// Whether the conversation has a task record, which fixes its mode and folder.
    pub(super) fn has_record(&self, id: Uuid) -> bool {
        self.snapshots.contains_key(&id)
    }

    /// The conversation whose task is running.
    pub(super) fn running(&self) -> Option<Uuid> {
        self.active.as_ref().map(|active| active.conversation_id)
    }

    pub(super) fn new(data_root: PathBuf) -> Self {
        Self {
            active: None,
            snapshots: HashMap::new(),
            data_root,
            recovery_started: false,
            recovery: None,
        }
    }
}

impl AgentUiState {
    pub(super) fn is_recovering(&self) -> bool {
        self.recovery.is_some()
    }
}

impl TacetaApp {
    pub(super) fn show_agent_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        ui.heading(text(
            language,
            "作業とコンパクション",
            "Agent work and compaction",
        ));
        ui.label(text(language,
            "作業モードでは履歴の原文、ユーザーの指示、作業状態をこのMacに保存します。コンテキストが満ちる前に背景を要約し、指示と未完了の状態を保持して続行します。",
            "Agent mode saves original events, user instructions and work state on this Mac. Before context fills up, it summarizes background while retaining instructions and unfinished work."));
        ui.add_enabled_ui(!self.is_generating(), |ui| {
            ui.horizontal(|ui| {
                ui.label(text(
                    language,
                    "1回の実行の推論回数上限",
                    "Inference calls per run",
                ));
                ui.add(egui::DragValue::new(&mut self.state.agent_max_steps).range(1..=1_000));
            });
            ui.horizontal(|ui| {
                ui.label(text(
                    language,
                    "1回の実行時間（分）",
                    "Run duration (minutes)",
                ));
                let mut minutes = self.state.agent_max_duration_secs / 60;
                if ui
                    .add(egui::DragValue::new(&mut minutes).range(1..=720))
                    .changed()
                {
                    self.state.agent_max_duration_secs = minutes * 60;
                }
            });
        });
        ui.label(RichText::new(text(language,
            "要約の推論も上限に含めます。上限到達と中断は完了と区別して保存し、画面の「再開」から続けられます。",
            "Compaction calls count toward the limit. Limit reached and interrupted runs are saved separately from completed work and can be resumed.")).small().weak());
        ui.add_space(20.0);
    }

    fn agent_can_resume(&self) -> bool {
        self.agent_ui.snapshots.get(&self.state.active_conversation_id).is_some_and(|snapshot| {
            !matches!(snapshot.status, SessionStatus::Completed | SessionStatus::Running | SessionStatus::AwaitingApproval)
        })
    }

    pub(super) fn agent_status_visible(&self) -> bool {
        self.state.active_conversation().agent_enabled
            && (self.agent_ui.running() == Some(self.state.active_conversation_id) || self.agent_can_resume())
    }

    /// A running task's progress, or that it stopped short and can resume.
    pub(super) fn show_agent_status(&mut self, ui: &mut Ui) {
        let language = self.language();
        let id = self.state.active_conversation_id;
        if let Some(active) = self.agent_ui.active.as_ref().filter(|active| active.conversation_id == id) {
            ui.spinner();
            ui.add(egui::Label::new(RichText::new(&active.progress).small().weak()).truncate());
        } else if self.agent_can_resume() {
            let status = self.agent_ui.snapshots.get(&id).map(|snapshot| snapshot.status.clone());
            if let Some(status) = status {
                ui.label(RichText::new(status_label(language, &status)).small().weak());
            }
            if ui.add_enabled(!self.is_generating(), Button::new(text(language, "再開", "Resume"))).clicked() {
                self.start_agent_run(true);
            }
        }
    }

    pub(super) fn show_agent_menu_items(&mut self, ui: &mut Ui) {
        let language = self.language();
        let has_record = self.agent_ui.has_record(self.state.active_conversation_id);
        if ui
            .add_enabled(has_record && !self.is_generating(), Button::new(text(language, "コンテキストを整理", "Compact context")))
            .clicked()
        {
            self.start_agent_action(true, true);
        }
    }

    pub(super) fn start_agent_run(&mut self, resume: bool) {
        self.start_agent_action(resume, false);
    }

    fn start_agent_action(&mut self, resume: bool, compact_only: bool) {
        if self.state.inference_provider == InferenceProvider::Gpt {
            return;
        }
        if self.is_generating()
            || self.model_unload_result.is_some()
            || self.agent_ui.recovery.is_some()
        {
            return;
        }
        match self.synchronize_auto_ollama_endpoint() {
            Ok(true) => {
                self.refresh_models();
                self.agent_notice(
                    "Ollamaの接続先が変わりました。モデル一覧の更新後に送信してください。",
                    "The Ollama endpoint changed. Send after the model list refreshes.",
                );
                return;
            }
            Err(error) => {
                self.handle_ollama_endpoint_error(error);
                return;
            }
            Ok(false) => {}
        }
        let language = self.language();
        let Some(model) = self.selected_model().cloned() else {
            self.agent_notice(
                "利用するモデルを選択してください。",
                "Select a model before starting.",
            );
            return;
        };
        if !model.tools && !compact_only {
            self.agent_notice(
                "作業モードにはツール対応を確認できたモデルが必要です。",
                "Agent mode requires a model with confirmed tool support.",
            );
            return;
        }
        let Some(workspace) = self.state.active_conversation().workspace.clone() else {
            self.agent_notice(
                "フォルダーを選んでください。",
                "Choose a folder.",
            );
            return;
        };
        if !self.state.pending_attachments.is_empty() {
            self.agent_notice(
                "作業モードでは添付を外し、作業フォルダー内のファイルを指定してください。",
                "Remove attachments and refer to files inside the workspace in Agent mode.",
            );
            return;
        }
        if !resume && self.state.draft.trim().is_empty() {
            return;
        }
        let data_root = self.agent_ui.data_root.clone();
        let id = self.state.active_conversation_id;
        let prior = importable_messages(&self.state.active_conversation().messages);
        // Turns another engine answered since the task record was last
        // written back are imported into it before the run.
        let (external, imported_external) = {
            let conversation = self.state.active_conversation();
            let unseen = conversation
                .task_seen_messages
                .and_then(|seen| conversation.messages.get(seen..))
                .unwrap_or_default();
            external_records(unseen)
        };
        let prompt = if resume {
            None
        } else {
            Some(std::mem::take(&mut self.state.draft))
        };
        if let Some(prompt) = &prompt {
            let conversation = self.state.active_conversation_mut();
            if conversation.should_generate_title() {
                conversation.title = conversation_title(prompt, "Taceta");
            }
        }
        let config = RunConfig {
            model: model.name,
            thinking: self.selected_thinking_mode(),
            context_length: self
                .state
                .context_length
                .min(model.context_length.unwrap_or(self.state.context_length)),
            prompt,
            max_steps: self.state.agent_max_steps,
            max_duration_secs: self.state.agent_max_duration_secs,
            compact_only,
        };
        let submitted_prompt = config.prompt.clone();
        let backend: Arc<dyn AgentModel> = match self.state.inference_provider {
            InferenceProvider::Ollama => {
                self.ollama_used = true;
                Arc::new(OllamaClient::new(self.ollama_endpoint.clone()))
            }
            InferenceProvider::Grok | InferenceProvider::Gpt | InferenceProvider::Claude => return,
        };
        let (event_tx, events) = mpsc::unbounded_channel();
        let (approval_tx, approval_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let task = self.runtime.spawn(async move {
            let preparation = tokio::task::spawn_blocking(move || {
                let (mut session, _created) =
                    AgentSession::open_or_create(&data_root, id, &workspace)?;
                if session.is_untouched() && !prior.is_empty() {
                    session.import_messages(prior)?;
                } else if !external.is_empty() {
                    session.import_external_handoff(&taceta::agent::ExternalWorkHandoff {
                        source: "Taceta conversation".into(),
                        source_session_id: id.to_string(),
                        workspace: workspace.clone(),
                        records: external,
                    })?;
                }
                Ok::<_, String>(session)
            })
            .await
            .map_err(|error| format!("Could not open the task: {error}"));
            let outcome = match preparation {
                Ok(Ok(session)) => {
                    session
                        .run(config, backend, event_tx, approval_rx, cancel_rx)
                        .await
                }
                Ok(Err(error)) | Err(error) => Err(error),
            };
            let _ = result_tx.send(outcome);
        });
        self.agent_ui.active = Some(ActiveAgent {
            task,
            conversation_id: id,
            events,
            result,
            cancel,
            approvals: approval_tx,
            approval: None,
            content: String::new(),
            thinking: String::new(),
            progress: text(language, "作業を開始しています…", "Starting task…").into(),
            submitted_prompt,
            imported_external,
        });
        self.notice = None;
        self.scroll_to_bottom = true;
        self.jump_to_latest = true;
    }

    fn agent_notice(&mut self, ja: &str, en: &str) {
        self.notice = Some(Notice {
            kind: NoticeKind::Warning,
            text: text(self.language(), ja, en).into(),
        });
    }

    pub(super) fn stop_agent_run(&mut self) {
        let language = self.language();
        if let Some(active) = self.agent_ui.active.as_mut() {
            let _ = active.cancel.send(true);
            active.approval = None;
            active.progress = text(language, "実行を停止しています…", "Stopping task…").into();
        }
    }

    pub(super) fn abort_agent_on_exit(&mut self) {
        if let Some(active) = self.agent_ui.active.take() {
            let _ = active.cancel.send(true);
            active.task.abort();
        }
    }

    fn begin_agent_recovery(&mut self) {
        if self.agent_ui.recovery_started {
            return;
        }
        self.agent_ui.recovery_started = true;
        let root = self.agent_ui.data_root.clone();
        let (tx, rx) = std_mpsc::channel();
        self.runtime.spawn_blocking(move || {
            let snapshots = match AgentSession::list_ids(&root) {
                Ok(ids) => ids
                    .into_iter()
                    .map(|id| AgentSession::open(&root, id).map(|session| session.snapshot()))
                    .collect(),
                Err(error) => vec![Err(error)],
            };
            let _ = tx.send(snapshots);
        });
        self.agent_ui.recovery = Some(rx);
    }

    pub(super) fn drain_agent_work(&mut self) {
        self.begin_agent_recovery();
        if let Some(receiver) = &self.agent_ui.recovery {
            let received = match receiver.try_recv() {
                Ok(snapshots) => Some(snapshots),
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    Some(vec![Err("Task recovery stopped".into())])
                }
                Err(std_mpsc::TryRecvError::Empty) => None,
            };
            if let Some(snapshots) = received {
                self.agent_ui.recovery = None;
                for snapshot in snapshots {
                    match snapshot {
                        Ok(snapshot) => self.remember_agent_snapshot(snapshot, &[]),
                        Err(error) => {
                            self.notice = Some(Notice {
                                kind: NoticeKind::Error,
                                text: error,
                            })
                        }
                    }
                }
            }
        }
        let mut events = Vec::new();
        let mut result = None;
        if let Some(active) = self.agent_ui.active.as_mut() {
            while let Ok(event) = active.events.try_recv() {
                events.push(event);
            }
            result = match active.result.try_recv() {
                Ok(result) => Some(result),
                Err(std_mpsc::TryRecvError::Disconnected) => Some(Err("Task stopped before its outcome was recorded; reopen the task to recover its history".into())),
                Err(std_mpsc::TryRecvError::Empty) => None,
            };
        }
        for event in events {
            self.apply_agent_event(event);
        }
        if let Some(result) = result {
            if result.is_err() {
                if let Some(active) = self.agent_ui.active.as_ref() {
                    if active.conversation_id == self.state.active_conversation_id
                        && self.state.draft.is_empty()
                    {
                        if let Some(prompt) = &active.submitted_prompt {
                            let recorded = self
                                .agent_ui
                                .snapshots
                                .get(&active.conversation_id)
                                .is_some_and(|snapshot| {
                                    snapshot.user_instructions.contains(prompt)
                                });
                            if !recorded {
                                self.state.draft = prompt.clone();
                            }
                        }
                    }
                }
            }
            let imported_external = self
                .agent_ui
                .active
                .take()
                .map(|active| active.imported_external)
                .unwrap_or_default();
            match result {
                Ok(snapshot) => {
                    // A turn that simply finished needs no announcement; the answer is there.
                    let finished = snapshot.status == SessionStatus::Completed && snapshot.error.is_none();
                    let status = status_label(self.language(), &snapshot.status).to_owned();
                    self.notice = (!finished).then(|| Notice {
                        kind: if matches!(snapshot.status, SessionStatus::Failed) {
                            NoticeKind::Error
                        } else {
                            NoticeKind::Info
                        },
                        text: snapshot.error.clone().unwrap_or(status),
                    });
                    self.remember_agent_snapshot(snapshot, &imported_external);
                }
                Err(error) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: error,
                    })
                }
            }
        }
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Delta(delta) => {
                if let Some(active) = self.agent_ui.active.as_mut() {
                    match delta {
                        ModelDelta::Content(text) => active.content.push_str(&text),
                        ModelDelta::Thinking(text) => active.thinking.push_str(&text),
                    }
                }
            }
            AgentEvent::Progress(progress) => {
                if let Some(active) = self.agent_ui.active.as_mut() {
                    active.progress = progress;
                }
            }
            AgentEvent::ApprovalRequired(request) => {
                if let Some(active) = self.agent_ui.active.as_mut() {
                    active.approval = Some(PendingApproval {
                        view: ApprovalView::from_details(&request.details),
                        request,
                        shown_at: std::time::Instant::now(),
                    });
                }
            }
            AgentEvent::Snapshot(snapshot) => {
                if let Some(active) = self.agent_ui.active.as_mut() {
                    if snapshot
                        .messages
                        .iter()
                        .rev()
                        .find(|message| message.role == AgentRole::Assistant)
                        .is_some_and(|message| message.content == active.content)
                    {
                        active.content.clear();
                        active.thinking.clear();
                    }
                }
                let imported = self
                    .agent_ui
                    .active
                    .as_ref()
                    .map(|active| active.imported_external.clone())
                    .unwrap_or_default();
                self.remember_agent_snapshot(snapshot, &imported);
            }
        }
        self.scroll_to_bottom = true;
    }

    /// Writes the task record back into the conversation. `imported` names
    /// the messages a run has just imported into that record.
    pub(super) fn remember_agent_snapshot(&mut self, snapshot: SessionSnapshot, imported: &[Uuid]) {
        let id = snapshot.id;
        if !self
            .state
            .conversations
            .iter()
            .any(|conversation| conversation.id == id)
        {
            self.state.conversations.insert(
                0,
                crate::persistence::Conversation {
                    id,
                    title: conversation_title(&snapshot.goal, "Taceta"),
                    agent_enabled: true,
                    workspace: Some(snapshot.workspace.clone()),
                    provider: Some(InferenceProvider::Ollama),
                    ..Default::default()
                },
            );
        }
        if let Some(conversation) = self
            .state
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == id)
        {
            // The conversation's messages are the record. The task record is
            // written back only while Taceta's own runner is the engine in
            // task mode, and turns it has not received are kept after it.
            if conversation.agent_enabled && conversation.provider.unwrap_or_default() == InferenceProvider::Ollama {
                conversation.workspace = Some(snapshot.workspace.clone());
                // Before the first run of a conversation that began elsewhere,
                // none of its messages are in the record yet.
                let in_record = conversation
                    .task_seen_messages
                    .unwrap_or(if imported.is_empty() { conversation.messages.len() } else { 0 })
                    .min(conversation.messages.len());
                let not_in_record: Vec<ChatMessage> = conversation.messages[in_record..]
                    .iter()
                    .filter(|message| !imported.contains(&message.id))
                    .cloned()
                    .collect();
                conversation.messages = snapshot
                    .messages
                    .iter()
                    .enumerate()
                    .filter_map(|(index, message)| transcript_message(id, index, message))
                    .collect();
                // Record entries placed ahead of messages that stay outside
                // the record move those messages. Positions other engines
                // and the summary remembered past that point no longer hold,
                // so they fall back to the last one that does: a turn may be
                // handed over twice, never skipped.
                if !not_in_record.is_empty() && conversation.messages.len() != in_record {
                    for session in conversation.cli_sessions.values_mut() {
                        session.seen_messages = session.seen_messages.min(in_record);
                    }
                    if let Some(summary) = conversation.summary.as_mut() {
                        summary.covers_messages = summary.covers_messages.min(in_record);
                    }
                }
                conversation.task_seen_messages = Some(conversation.messages.len());
                conversation.messages.extend(not_in_record);
            }
        }
        self.agent_ui.snapshots.insert(id, snapshot);
    }

    pub(super) fn show_agent_chat(&mut self, root_ui: &mut Ui) {
        let id = self.state.active_conversation_id;
        let language = self.language();
        self.show_transcript(root_ui, "agent-transcript", |app, ui| {
            if let Some(snapshot) = app.agent_ui.snapshots.get(&id) {
                if snapshot.summary.is_some() {
                    ui.label(
                        RichText::new(text(
                            language,
                            "コンパクション済み・原文履歴は保存されています。",
                            "Context compacted. Original history is preserved.",
                        ))
                        .small()
                        .weak(),
                    );
                }
                for (index, message) in snapshot.messages.iter().enumerate() {
                    if let Some(transcript) = transcript_message(id, index, message) {
                        app.show_message(ui, &transcript, false);
                    } else if message.role == AgentRole::Tool || external_tool_evidence(message).is_some() {
                        super::show_action_row(
                            ui,
                            ("agent-tool", id, index),
                            super::ActionMark::Done,
                            message.tool_name.as_deref().unwrap_or("Tool result"),
                            false,
                            |ui| {
                                ui.label(external_tool_evidence(message).unwrap_or_else(|| message.content.clone()));
                            },
                        );
                    }
                }
            } else {
                for message in &app.state.active_conversation().messages {
                    app.show_message(ui, message, false);
                }
                ui.label(text(
                    language,
                    "フォルダーとモデルを選び、実行したいことを入力してください。",
                    "Choose a folder and model, then describe the work.",
                ));
            }
            if let Some(active) = app.agent_ui.active.as_ref() {
                if active.conversation_id == id
                    && (!active.content.is_empty() || !active.thinking.is_empty())
                {
                    let mut message = ChatMessage::new_assistant(active.content.clone());
                    message.id = id;
                    message.thinking = active.thinking.clone();
                    app.show_message(ui, &message, true);
                }
            }
        });
    }

    pub(super) fn show_agent_approval(&mut self, ctx: &Context) {
        let language = self.language();
        let Some(active) = self.agent_ui.active.as_ref() else {
            return;
        };
        let Some(pending) = active.approval.as_ref() else {
            return;
        };
        let request = &pending.request;
        let settled = pending.shown_at.elapsed() >= APPROVAL_SETTLE;
        if !settled {
            ctx.request_repaint_after(APPROVAL_SETTLE);
        }
        let palette_colors = |ui: &Ui| {
            let palette = theme::palette(ui);
            (palette.success, palette.error, palette.placeholder_text)
        };
        let mut decision = None;
        egui::Window::new(text(language, "実行内容の確認", "Approve this action"))
            .id(egui::Id::new("taceta-agent-approval"))
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(&request.description);
                if let Some(conversation) = self
                    .state
                    .conversations
                    .iter()
                    .find(|conversation| conversation.id == active.conversation_id)
                {
                    ui.label(&conversation.title);
                    if let Some(workspace) = &conversation.workspace {
                        ui.monospace(workspace.display().to_string());
                    }
                }
                ScrollArea::both().max_height(440.0).show(ui, |ui| match &pending.view {
                    ApprovalView::Edit { path, created, lines } => {
                        let (added, removed, quiet) = palette_colors(ui);
                        ui.monospace(path);
                        if *created {
                            ui.label(RichText::new(text(language, "新しいファイル", "New file")).small().weak());
                        }
                        if lines.is_empty() {
                            ui.label(text(language, "内容の変更はありません。", "The content does not change."));
                        }
                        for line in lines {
                            match line {
                                DiffLine::Same(line) => ui.monospace(format!("  {line}")),
                                DiffLine::Removed(line) => ui.label(RichText::new(format!("- {line}")).monospace().color(removed)),
                                DiffLine::Added(line) => ui.label(RichText::new(format!("+ {line}")).monospace().color(added)),
                                DiffLine::Skipped(count) => ui.label(RichText::new(match language {
                                    AppShellLanguage::Japanese => format!("… 変更のない {count} 行"),
                                    AppShellLanguage::English => format!("… {count} unchanged lines"),
                                }).small().color(quiet)),
                            };
                        }
                    }
                    ApprovalView::Fields(fields) => {
                        egui::Grid::new("taceta-agent-approval-fields").num_columns(2).show(ui, |ui| {
                            for (name, value) in fields {
                                ui.label(RichText::new(name).weak());
                                ui.monospace(value);
                                ui.end_row();
                            }
                        });
                    }
                    ApprovalView::Text(details) => {
                        ui.monospace(details);
                    }
                });
                ui.add_enabled_ui(settled, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button(text(language, "この1回を許可", "Approve once"))
                            .clicked()
                        {
                            decision = Some(true);
                        }
                        if ui.button(text(language, "拒否", "Deny")).clicked() {
                            decision = Some(false);
                        }
                    });
                });
            });
        if let Some(approved) = decision {
            self.decide_agent_approval(approved);
        }
    }

    fn decide_agent_approval(&mut self, approved: bool) {
        if let Some(active) = self.agent_ui.active.as_mut() {
            if let Some(pending) = active.approval.take() {
                let _ = active.approvals.send(ApprovalDecision {
                    id: pending.request.id,
                    approved,
                });
            }
        }
    }

    pub(super) fn archive_agent_chats(&mut self, ids: &[Uuid]) -> bool {
        let root = self.agent_ui.data_root.clone();
        for id in ids {
            if let Err(error) = AgentSession::archive(&root, *id) {
                self.notice = Some(Notice {
                    kind: NoticeKind::Error,
                    text: error,
                });
                return false;
            }
            self.agent_ui.snapshots.remove(id);
        }
        true
    }
}

fn importable_messages(messages: &[ChatMessage]) -> Vec<AgentMessage> {
    messages
        .iter()
        .filter_map(|message| {
            if message.interrupted {
                return None;
            }
            let role = match message.role {
                Role::User => AgentRole::User,
                Role::Assistant => AgentRole::Assistant,
                Role::System => return None,
            };
            let mut content = message.content.clone();
            for attachment in &message.attachments {
                if let AttachmentPayload::Text(text) = &attachment.payload {
                    content.push_str(&format!("\n\nUser attachment: {}\n{text}", attachment.name));
                }
            }
            Some(AgentMessage {
                role,
                content,
                tool_calls: Vec::new(),
                tool_call_id: None,
                tool_name: None,
            })
        })
        .collect()
}

/// The messages of other engines as records the task runner can import, with
/// the ids of the messages they came from. Interrupted output is left out.
fn external_records(messages: &[ChatMessage]) -> (Vec<taceta::agent::ExternalWorkRecord>, Vec<Uuid>) {
    use taceta::agent::{ExternalWorkRecord, ExternalWorkRole};
    messages
        .iter()
        .filter(|message| !message.interrupted)
        .filter_map(|message| {
            let role = match message.role {
                Role::User => ExternalWorkRole::User,
                Role::Assistant => ExternalWorkRole::Assistant,
                Role::System => return None,
            };
            let mut content = message.content.clone();
            for action in &message.actions {
                content.push_str(&format!(
                    "\n\n[Action {}: {}]",
                    if action.succeeded { "done" } else { "failed" },
                    action.title
                ));
            }
            Some((ExternalWorkRecord { id: message.id.to_string(), role, content }, message.id))
        })
        .unzip()
}

fn transcript_message(id: Uuid, index: usize, message: &AgentMessage) -> Option<ChatMessage> {
    let role = match message.role {
        AgentRole::User => Role::User,
        AgentRole::Assistant => Role::Assistant,
        AgentRole::System | AgentRole::Tool => return None,
    };
    let evidence = external_evidence(message);
    if evidence.as_ref().is_some_and(|value| value["record_kind"] == "ToolResult") { return None; }
    let content = evidence.as_ref().and_then(|value| value["content"].as_str()).unwrap_or(&message.content);
    let mut result = ChatMessage::new(role, content);
    result.id = Uuid::from_u128(id.as_u128() ^ ((index as u128) + 1));
    Some(result)
}

fn external_evidence(message: &AgentMessage) -> Option<serde_json::Value> {
    if message.role != AgentRole::Assistant { return None; }
    let value: serde_json::Value = serde_json::from_str(&message.content).ok()?;
    (value["external_work_evidence"] == true && value["content"].is_string()).then_some(value)
}

fn external_tool_evidence(message: &AgentMessage) -> Option<String> {
    let evidence = external_evidence(message)?;
    (evidence["record_kind"] == "ToolResult").then(|| evidence["content"].as_str().unwrap().to_owned())
}

fn status_label(language: AppShellLanguage, status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => text(language, "実行中", "Running"),
        SessionStatus::AwaitingApproval => text(language, "確認待ち", "Awaiting approval"),
        SessionStatus::Completed => text(language, "完了", "Completed"),
        SessionStatus::Interrupted => {
            text(language, "中断・再開できます", "Interrupted · resumable")
        }
        SessionStatus::LimitReached => text(
            language,
            "上限到達・再開できます",
            "Limit reached · resumable",
        ),
        SessionStatus::Failed => text(
            language,
            "失敗・状態を確認して再開",
            "Failed · review and resume",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_manager_tests::{RecordingServices, test_app};

    #[test]
    fn a_conversation_now_answered_by_another_engine_is_not_overwritten_by_its_old_task_record() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        app.state.switch_provider(InferenceProvider::Gpt);
        let id = app.state.active_conversation_id;
        app.state.active_conversation_mut().messages.push(ChatMessage::new_assistant("Current GPT result"));
        app.remember_agent_snapshot(SessionSnapshot {
            id, workspace: PathBuf::from("/fixture/old-workspace"), status: SessionStatus::Interrupted,
            goal: "Earlier task".into(), user_instructions: vec!["Earlier instruction".into()],
            work_state: Default::default(), messages: vec![AgentMessage {
                role: AgentRole::Assistant, content: "Old result".into(), tool_calls: vec![],
                tool_call_id: None, tool_name: None,
            }], error: None, event_count: 1, summary: None, latest_model: Some("grok-test".into()),
        }, &[]);
        assert_eq!(app.state.active_conversation().messages[0].content, "Current GPT result");
        assert!(app.state.active_conversation().workspace.is_none());
    }

    #[test]
    fn task_record_replaces_the_transcript_only_while_the_conversation_is_in_task_mode() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let id = app.state.active_conversation_id;
        let snapshot = || SessionSnapshot {
            id, workspace: PathBuf::from("/fixture/workspace"), status: SessionStatus::Interrupted,
            goal: "Task".into(), user_instructions: vec![], work_state: Default::default(),
            messages: vec![AgentMessage {
                role: AgentRole::Assistant, content: "Task result".into(), tool_calls: vec![],
                tool_call_id: None, tool_name: None,
            }], error: None, event_count: 1, summary: None, latest_model: None,
        };
        let contents = |app: &TacetaApp| app.state.active_conversation().messages.iter()
            .map(|message| message.content.clone()).collect::<Vec<_>>();

        app.state.active_conversation_mut().agent_enabled = false;
        app.state.active_conversation_mut().messages.push(ChatMessage::new(Role::User, "Chat-only turn"));
        app.remember_agent_snapshot(snapshot(), &[]);
        assert_eq!(contents(&app), ["Chat-only turn"]);
        assert!(app.agent_ui.snapshots.contains_key(&id));

        app.state.active_conversation_mut().agent_enabled = true;
        app.remember_agent_snapshot(snapshot(), &[]);
        assert_eq!(contents(&app), ["Task result"]);
    }

    #[test]
    fn turns_from_other_engines_survive_the_task_record_and_join_it_once_imported() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let id = app.state.active_conversation_id;
        let record = |contents: &[&str]| SessionSnapshot {
            id, workspace: PathBuf::from("/fixture/workspace"), status: SessionStatus::Completed,
            goal: "Task".into(), user_instructions: vec![], work_state: Default::default(),
            messages: contents.iter().map(|content| AgentMessage {
                role: AgentRole::Assistant, content: (*content).into(), tool_calls: vec![],
                tool_call_id: None, tool_name: None,
            }).collect(),
            error: None, event_count: 1, summary: None, latest_model: None,
        };
        let contents = |app: &TacetaApp| app.state.active_conversation().messages.iter()
            .map(|message| message.content.clone()).collect::<Vec<_>>();
        app.state.active_conversation_mut().agent_enabled = true;
        app.remember_agent_snapshot(record(&["task answer"]), &[]);
        assert_eq!(app.state.active_conversation().task_seen_messages, Some(1));

        // Another engine answers two turns in the same conversation.
        app.state.active_conversation_mut().provider = Some(InferenceProvider::Claude);
        let elsewhere = [ChatMessage::new_user("asked elsewhere"), ChatMessage::new_assistant("answered elsewhere")];
        let ids: Vec<Uuid> = elsewhere.iter().map(|message| message.id).collect();
        app.state.active_conversation_mut().messages.extend(elsewhere);
        // While that engine is selected, the task record is not written back at all.
        app.remember_agent_snapshot(record(&["task answer"]), &[]);
        assert_eq!(contents(&app), ["task answer", "asked elsewhere", "answered elsewhere"]);

        // Back on the local runner, a restart keeps the turns it has not received.
        app.state.active_conversation_mut().provider = Some(InferenceProvider::Ollama);
        app.remember_agent_snapshot(record(&["task answer"]), &[]);
        assert_eq!(contents(&app), ["task answer", "asked elsewhere", "answered elsewhere"]);

        // A run imports them; the record then holds them and they are not kept twice.
        let (records, imported) = external_records(&app.state.active_conversation().messages[1..]);
        assert_eq!(imported, ids);
        assert_eq!(records.iter().map(|record| record.content.as_str()).collect::<Vec<_>>(), ["asked elsewhere", "answered elsewhere"]);
        app.remember_agent_snapshot(record(&["task answer", "asked elsewhere", "answered elsewhere", "next task answer"]), &imported);
        assert_eq!(contents(&app), ["task answer", "asked elsewhere", "answered elsewhere", "next task answer"]);
        assert_eq!(app.state.active_conversation().task_seen_messages, Some(4));

        // A stopped turn from another engine is never imported. When later
        // record entries land ahead of it, what that engine had seen is
        // counted only up to where positions still hold.
        let conversation = app.state.active_conversation_mut();
        let mut stopped = ChatMessage::new_assistant("half");
        stopped.interrupted = true;
        conversation.messages.push(stopped);
        conversation.cli_sessions.insert(
            InferenceProvider::Claude,
            crate::persistence::CliSession { session_id: "s".into(), seen_messages: 5 },
        );
        conversation.summary = Some(taceta::prior_context::Summary { text: "so far".into(), covers_messages: 5 });
        app.remember_agent_snapshot(record(&["task answer", "asked elsewhere", "answered elsewhere", "next task answer", "one more"]), &[]);
        assert_eq!(contents(&app), ["task answer", "asked elsewhere", "answered elsewhere", "next task answer", "one more", "half"]);
        let conversation = app.state.active_conversation();
        assert_eq!(conversation.cli_sessions[&InferenceProvider::Claude].seen_messages, 4);
        assert_eq!(conversation.summary.as_ref().unwrap().covers_messages, 4);
    }

    #[test]
    fn an_edit_proposal_is_shown_as_the_lines_that_change() {
        let before = (1..=20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let after = before.replace("line 10", "line ten").replace("line 20", "line 20\nline 21");
        let details = serde_json::json!({"workspace":"/w","path":"src/a.rs","before":before,"after":after}).to_string();
        let ApprovalView::Edit { path, created, lines } = ApprovalView::from_details(&details) else {
            panic!("an edit view was expected");
        };
        assert_eq!(path, "src/a.rs");
        assert!(!created);
        let changed: Vec<&DiffLine> = lines.iter().filter(|line| matches!(line, DiffLine::Removed(_) | DiffLine::Added(_))).collect();
        assert_eq!(changed, [
            &DiffLine::Removed("line 10".into()),
            &DiffLine::Added("line ten".into()),
            &DiffLine::Added("line 21".into()),
        ]);
        assert_eq!(lines.first(), Some(&DiffLine::Skipped(6)));
        assert!(lines.contains(&DiffLine::Same("line 9".into())));
        assert!(!lines.contains(&DiffLine::Same("line 1".into())));

        let created = serde_json::json!({"workspace":"/w","path":"new.txt","before":null,"after":"hi\n"}).to_string();
        assert_eq!(
            ApprovalView::from_details(&created),
            ApprovalView::Edit { path: "new.txt".into(), created: true, lines: vec![DiffLine::Added("hi".into())] }
        );
        let command = serde_json::json!({"command":"cargo test","network":"denied"}).to_string();
        assert_eq!(
            ApprovalView::from_details(&command),
            ApprovalView::Fields(vec![("command".into(), "cargo test".into()), ("network".into(), "denied".into())])
        );
        assert_eq!(ApprovalView::from_details("old: a"), ApprovalView::Text("old: a".into()));
    }

    #[test]
    fn approval_button_is_one_shot_and_stop_is_independent_of_trace_visibility() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_event_tx, events) = mpsc::unbounded_channel();
        let (_result_tx, result) = std_mpsc::channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let (approvals, mut decisions) = mpsc::unbounded_channel();
        let task = app.runtime.spawn(std::future::pending());
        app.agent_ui.active = Some(ActiveAgent {
            task,
            conversation_id: app.state.active_conversation_id,
            events,
            result,
            cancel,
            approvals,
            approval: None,
            content: String::new(),
            thinking: String::new(),
            progress: String::new(),
            submitted_prompt: None,
            imported_external: Vec::new(),
        });
        let request_id = Uuid::new_v4();
        app.apply_agent_event(AgentEvent::ApprovalRequired(ApprovalRequest {
            id: request_id,
            tool_call_id: "edit-3".into(),
            description: "Edit a.rs".into(),
            details: "old: a\nnew: b".into(),
        }));
        assert!(decisions.try_recv().is_err());
        app.decide_agent_approval(true);
        app.decide_agent_approval(true);
        let decision = decisions.try_recv().unwrap();
        assert_eq!(decision.id, request_id);
        assert!(decision.approved);
        assert!(decisions.try_recv().is_err());
        app.state.show_thinking_trace = false;
        app.apply_agent_event(AgentEvent::Delta(ModelDelta::Thinking("working".into())));
        app.apply_agent_event(AgentEvent::Delta(ModelDelta::Content(
            "visible answer".into(),
        )));
        assert_eq!(
            app.agent_ui.active.as_ref().unwrap().content,
            "visible answer"
        );
        assert_eq!(app.agent_ui.active.as_ref().unwrap().thinking, "working");
        app.stop_generation();
        assert!(*cancel_rx.borrow());
        assert!(app.is_generating()); // The UI waits for a recorded exit.
    }

    #[test]
    fn chat_import_and_recovered_work_preserve_input_without_replaying_thinking() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let original_id = app.state.active_conversation_id;
        let mut final_answer = ChatMessage::new_assistant("answer");
        final_answer.thinking = "private trace".into();
        let mut interrupted = ChatMessage::new_assistant("partial output");
        interrupted.interrupted = true;
        let imported = importable_messages(&[
            ChatMessage::new_user("Preserve src/main.rs and port 4317"),
            final_answer,
            interrupted,
        ]);
        assert_eq!(imported.len(), 2);
        assert!(
            !serde_json::to_string(&imported)
                .unwrap()
                .contains("private trace")
        );
        assert!(
            !serde_json::to_string(&imported)
                .unwrap()
                .contains("partial output")
        );
        let recovered_id = Uuid::new_v4();
        app.remember_agent_snapshot(SessionSnapshot {
            id: recovered_id,
            workspace: PathBuf::from("/fixture/workspace"),
            status: SessionStatus::Interrupted,
            goal: "Continue the implementation".into(),
            user_instructions: vec!["Preserve src/main.rs and port 4317".into()],
            work_state: Default::default(),
            messages: imported,
            error: None,
            event_count: 7,
            summary: None,
            latest_model: Some("grok-test".into()),
        }, &[]);
        assert_eq!(app.state.active_conversation_id, original_id);
        assert!(
            app.state
                .conversations
                .iter()
                .any(|conversation| conversation.id == original_id)
        );
        let recovered = app
            .state
            .conversations
            .iter()
            .find(|conversation| conversation.id == recovered_id)
            .unwrap();
        assert!(recovered.agent_enabled);
        assert_eq!(
            recovered.workspace.as_deref(),
            Some(std::path::Path::new("/fixture/workspace"))
        );
        assert_eq!(
            recovered.messages[0].content,
            "Preserve src/main.rs and port 4317"
        );
        assert!(app.agent_ui.active.is_none()); // Recovery never starts inference.
    }
}
