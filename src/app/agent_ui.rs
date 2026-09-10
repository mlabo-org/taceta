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
    approval: Option<ApprovalRequest>,
    content: String,
    thinking: String,
    progress: String,
    submitted_prompt: Option<String>,
}

pub(super) struct AgentUiState {
    pub active: Option<ActiveAgent>,
    snapshots: HashMap<Uuid, SessionSnapshot>,
    data_root: Result<PathBuf, String>,
    recovery_started: bool,
    recovery: Option<std_mpsc::Receiver<Vec<Result<SessionSnapshot, String>>>>,
}

impl Default for AgentUiState {
    fn default() -> Self {
        #[cfg(not(test))]
        let data_root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library/Application Support/Taceta/AgentSessions"))
            .ok_or_else(|| "The current user's home directory is unavailable".to_owned());
        #[cfg(test)]
        let data_root = Ok(std::env::temp_dir().join(format!("taceta-ui-{}", Uuid::new_v4())));
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

    pub(super) fn show_agent_controls(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        Panel::top("taceta-agent-controls").show_inside(root_ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let enabled = self.state.active_conversation().agent_enabled;
                let mut selected = enabled;
                ui.add_enabled_ui(!self.is_generating(), |ui| {
                    ui.selectable_value(&mut selected, false, text(language, "チャット", "Chat"));
                    ui.selectable_value(&mut selected, true, text(language, "作業", "Agent"));
                });
                self.state.active_conversation_mut().agent_enabled = selected;
                self.show_provider_selector(ui);
                if !selected { return; }
                let id = self.state.active_conversation_id;
                let workspace = self.state.active_conversation().workspace.clone();
                let label = workspace.as_ref().and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| text(language, "作業フォルダーを選ぶ", "Choose workspace").into());
                let has_session = self.agent_ui.snapshots.contains_key(&id);
                if ui.add_enabled(!self.is_generating() && !has_session, Button::new(label))
                    .on_hover_text(workspace.as_ref().map(|path| path.display().to_string())
                        .unwrap_or_default()).clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.state.active_conversation_mut().workspace = Some(path);
                    }
                }
                if has_session {
                    ui.label(RichText::new(text(language, "フォルダー変更は新しい会話で行えます。", "Use a new chat to change workspace.")).small().weak());
                }
                let status = self.agent_ui.snapshots.get(&id).map(|snapshot| snapshot.status.clone());
                if let Some(status) = status {
                    ui.label(status_label(language, &status));
                    if !matches!(status, SessionStatus::Completed | SessionStatus::Running | SessionStatus::AwaitingApproval)
                        && ui.add_enabled(!self.is_generating(), Button::new(text(language, "再開", "Resume"))).clicked() {
                        self.start_agent_run(true);
                    }
                }
                if has_session && ui.add_enabled(!self.is_generating(), Button::new(text(language,
                    "コンテキストを整理", "Compact context"))).clicked() {
                    self.start_agent_action(true, true);
                }
            });
            if self.state.active_conversation().agent_enabled {
                ui.label(RichText::new(text(language,
                    "このフォルダーを読み取り、編集とコマンドは内容を確認してから実行します。コマンドの通信は無効です。",
                    "Reads this workspace. Edits and commands require approval of their exact contents. Commands have no network access.")).small().weak());
                if let Some(active) = self.agent_ui.active.as_ref() {
                    if active.conversation_id == self.state.active_conversation_id {
                        ui.horizontal_wrapped(|ui| { ui.spinner(); ui.label(&active.progress); });
                    }
                }
            }
        });
    }

    pub(super) fn start_agent_run(&mut self, resume: bool) {
        self.start_agent_action(resume, false);
    }

    fn start_agent_action(&mut self, resume: bool, compact_only: bool) {
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
                "作業フォルダーを選択してください。",
                "Choose the task's workspace.",
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
        let data_root = match &self.agent_ui.data_root {
            Ok(root) => root.clone(),
            Err(error) => {
                self.notice = Some(Notice {
                    kind: NoticeKind::Error,
                    text: error.clone(),
                });
                return;
            }
        };
        let id = self.state.active_conversation_id;
        let prior = importable_messages(&self.state.active_conversation().messages);
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
            InferenceProvider::Ollama => Arc::new(OllamaClient::new(self.ollama_endpoint.clone())),
            InferenceProvider::Grok => Arc::clone(&self.provider_ui.grok) as Arc<dyn AgentModel>,
        };
        let (event_tx, events) = mpsc::unbounded_channel();
        let (approval_tx, approval_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let task = self.runtime.spawn(async move {
            let preparation = tokio::task::spawn_blocking(move || {
                let (mut session, created) =
                    AgentSession::open_or_create(&data_root, id, &workspace)?;
                if created && !prior.is_empty() {
                    session.import_messages(prior)?;
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
        });
        self.notice = None;
        self.scroll_to_bottom = true;
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
        let Ok(root) = self.agent_ui.data_root.clone() else {
            return;
        };
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
                        Ok(snapshot) => self.remember_agent_snapshot(snapshot),
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
            self.agent_ui.active = None;
            match result {
                Ok(snapshot) => {
                    let status = status_label(self.language(), &snapshot.status).to_owned();
                    self.notice = Some(Notice {
                        kind: if matches!(snapshot.status, SessionStatus::Failed) {
                            NoticeKind::Error
                        } else {
                            NoticeKind::Info
                        },
                        text: snapshot.error.clone().unwrap_or(status),
                    });
                    self.remember_agent_snapshot(snapshot);
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
                    active.approval = Some(request);
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
                self.remember_agent_snapshot(snapshot);
            }
        }
        self.scroll_to_bottom = true;
    }

    fn remember_agent_snapshot(&mut self, snapshot: SessionSnapshot) {
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
            conversation.workspace = Some(snapshot.workspace.clone());
            conversation.messages = snapshot
                .messages
                .iter()
                .enumerate()
                .filter_map(|(index, message)| transcript_message(id, index, message))
                .collect();
        }
        self.agent_ui.snapshots.insert(id, snapshot);
    }

    pub(super) fn show_agent_chat(&mut self, root_ui: &mut Ui) {
        let id = self.state.active_conversation_id;
        let language = self.language();
        CentralPanel::default().show_inside(root_ui, |ui| {
            self.show_notice(ui);
            ScrollArea::vertical()
                .stick_to_bottom(self.scroll_to_bottom || self.is_generating())
                .show(ui, |ui| {
                    if let Some(snapshot) = self.agent_ui.snapshots.get(&id) {
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
                                self.show_message(ui, &transcript, false);
                            } else if message.role == AgentRole::Tool {
                                egui::CollapsingHeader::new(
                                    message.tool_name.as_deref().unwrap_or("Tool result"),
                                )
                                .id_salt(("agent-tool", id, index))
                                .show(ui, |ui| {
                                    ui.label(&message.content);
                                });
                            }
                        }
                    } else {
                        for message in &self.state.active_conversation().messages {
                            self.show_message(ui, message, false);
                        }
                        ui.label(text(
                            language,
                            "作業フォルダーとモデルを選び、実行したいことを入力してください。",
                            "Choose a workspace and model, then describe the work.",
                        ));
                    }
                    if let Some(active) = self.agent_ui.active.as_ref() {
                        if active.conversation_id == id
                            && (!active.content.is_empty() || !active.thinking.is_empty())
                        {
                            let mut message = ChatMessage::new_assistant(active.content.clone());
                            message.id = id;
                            message.thinking = active.thinking.clone();
                            self.show_message(ui, &message, true);
                        }
                    }
                });
        });
        self.scroll_to_bottom = false;
    }

    pub(super) fn show_agent_approval(&mut self, ctx: &Context) {
        let language = self.language();
        let Some(active) = self.agent_ui.active.as_ref() else {
            return;
        };
        let Some(request) = active.approval.as_ref() else {
            return;
        };
        let mut decision = None;
        egui::Window::new(text(language, "実行内容の確認", "Approve this action"))
            .id(egui::Id::new("taceta-agent-approval"))
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
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
                ScrollArea::both().max_height(440.0).show(ui, |ui| {
                    ui.monospace(&request.details);
                });
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
        if let Some(approved) = decision {
            self.decide_agent_approval(approved);
        }
    }

    fn decide_agent_approval(&mut self, approved: bool) {
        if let Some(active) = self.agent_ui.active.as_mut() {
            if let Some(request) = active.approval.take() {
                let _ = active.approvals.send(ApprovalDecision {
                    id: request.id,
                    approved,
                });
            }
        }
    }

    pub(super) fn archive_agent_chats(&mut self, ids: &[Uuid]) -> bool {
        let root = match &self.agent_ui.data_root {
            Ok(root) => root.clone(),
            Err(error) => {
                if ids.iter().any(|id| {
                    self.state.conversations.iter().any(|conversation| {
                        conversation.id == *id && conversation.workspace.is_some()
                    })
                }) {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: error.clone(),
                    });
                    return false;
                }
                return true;
            }
        };
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

fn transcript_message(id: Uuid, index: usize, message: &AgentMessage) -> Option<ChatMessage> {
    let role = match message.role {
        AgentRole::User => Role::User,
        AgentRole::Assistant => Role::Assistant,
        AgentRole::System | AgentRole::Tool => return None,
    };
    let mut result = ChatMessage::new(role, &message.content);
    result.id = Uuid::from_u128(id.as_u128() ^ ((index as u128) + 1));
    Some(result)
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
        });
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
