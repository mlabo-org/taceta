//! The task view for coding CLIs driven through `taceta::cli_agent`.
//!
//! The conversation's messages are the record. A CLI's own session is its
//! working context for that conversation: it is resumed while it is current,
//! and given the turns it missed when another engine answered in between.

use super::*;
use std::collections::{HashMap, VecDeque};
use taceta::{
    cli_agent::{
        ApprovalPolicy, ApprovalRequest, CliAgent, Control, Event, ModelOption, Question, Readiness,
        RunOutcome, RunRequest, RunStatus, ToolItem, ToolStatus, claude::ClaudeCode, codex::CodexCli,
        grok::GrokCli,
    },
    domain::ActionRecord,
    prior_context::with_prior_context,
};
use tokio::sync::watch;

/// Decisions are ignored briefly after a request appears, so a click meant
/// for the previous request cannot answer this one.
const APPROVAL_SETTLE: Duration = Duration::from_millis(600);

pub(super) struct ActiveCli {
    task: JoinHandle<()>,
    conversation_id: Uuid,
    assistant_id: Uuid,
    provider: InferenceProvider,
    events: mpsc::UnboundedReceiver<Event>,
    controls: mpsc::UnboundedSender<Control>,
    cancel: watch::Sender<bool>,
    result: std_mpsc::Receiver<RunOutcome>,
    tools: Vec<ToolItem>,
    approvals: VecDeque<(ApprovalRequest, std::time::Instant)>,
    questions: VecDeque<PendingQuestions>,
    session_id: Option<String>,
    /// The turn is conversation only; anything that asks is refused.
    no_tools: bool,
}

/// Questions the CLI asked together, with the answer typed so far for each.
struct PendingQuestions {
    id: Uuid,
    questions: Vec<Question>,
    answers: Vec<String>,
}

impl ActiveCli {
    pub(super) fn conversation(&self) -> Uuid {
        self.conversation_id
    }
}

pub(super) struct CliUiState {
    claude: Arc<dyn CliAgent>,
    codex: Arc<dyn CliAgent>,
    grok: Arc<dyn CliAgent>,
    pub(super) active: Option<ActiveCli>,
    readiness: HashMap<InferenceProvider, Result<Readiness, String>>,
    readiness_result: Option<(InferenceProvider, std_mpsc::Receiver<Result<Readiness, String>>)>,
    /// Where the CLIs run for a conversation without tools: an empty folder
    /// of Taceta's own, so no project is opened.
    chat_workspace: PathBuf,
}

impl CliUiState {
    /// `grok_approval_plugin` is where the Grok driver keeps the small plugin
    /// that makes its CLI ask before each action.
    pub(super) fn new(grok_approval_plugin: PathBuf) -> Self {
        Self {
            claude: Arc::new(ClaudeCode),
            codex: Arc::new(CodexCli::new()),
            chat_workspace: grok_approval_plugin.with_file_name("CliChatWorkspace"),
            grok: Arc::new(GrokCli::new(grok_approval_plugin)),
            active: None,
            readiness: HashMap::new(),
            readiness_result: None,
        }
    }

    /// The CLI behind `provider`, when that provider is a coding CLI.
    pub(super) fn agent(&self, provider: InferenceProvider) -> Option<Arc<dyn CliAgent>> {
        match provider {
            InferenceProvider::Claude => Some(Arc::clone(&self.claude)),
            InferenceProvider::Grok => Some(Arc::clone(&self.grok)),
            InferenceProvider::Gpt => Some(Arc::clone(&self.codex)),
            InferenceProvider::Ollama => None,
        }
    }
}

pub(super) fn model_descriptor(option: ModelOption) -> ModelDescriptor {
    let supported: Vec<ReasoningEffort> =
        option.efforts.iter().filter_map(|effort| ReasoningEffort::parse(effort)).collect();
    // The effort shown is the effort sent. Where the CLI names no default,
    // Taceta starts in the middle of the range it offers.
    let default = option
        .default_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .or_else(|| supported.get(supported.len() / 2).copied());
    ModelDescriptor {
        name: option.id,
        size: 0,
        thinking: if supported.is_empty() {
            ThinkingCapability::None
        } else {
            ThinkingCapability::Efforts { default, supported }
        },
        vision: option.images,
        tools: true,
        context_length: option.context_window.and_then(|window| u32::try_from(window).ok()),
        remote_host: None,
    }
}

impl TacetaApp {
    pub(super) fn is_cli_provider(&self) -> bool {
        self.cli_ui.agent(self.state.inference_provider).is_some()
    }

    pub(super) fn refresh_cli_models(&mut self, agent: Arc<dyn CliAgent>, epoch: u64) {
        let result_tx = self.model_result_tx.clone();
        let provider = self.state.inference_provider;
        let (readiness_tx, readiness_rx) = std_mpsc::channel();
        self.cli_ui.readiness_result = Some((provider, readiness_rx));
        self.runtime.spawn(async move {
            let readiness = agent.readiness().await;
            let models = match &readiness {
                Ok(_) => agent.models().await.map(|models| models.into_iter().map(model_descriptor).collect()),
                Err(error) => Err(error.clone()),
            };
            let listed = models.is_ok();
            let _ = readiness_tx.send(readiness);
            let _ = result_tx.send((epoch, models));
            // A CLI that answered from a stored list may announce a newer one.
            if listed {
                if let Some(refreshed) = agent.refreshed_models().await {
                    let _ = result_tx.send((epoch, Ok(refreshed.into_iter().map(model_descriptor).collect())));
                }
            }
        });
    }

    /// The CLI could not be checked, or is not signed in.
    fn cli_problem(&self) -> Option<String> {
        match self.cli_ui.readiness.get(&self.state.inference_provider)? {
            Ok(readiness) => (readiness.signed_in == Some(false)).then(|| text(
                self.language(),
                "CLIにサインインしていません。ターミナルでCLIを起動してサインインしてください。",
                "The CLI is not signed in. Start it in a terminal and sign in there.",
            ).to_owned()),
            Err(error) => Some(error.clone()),
        }
    }

    pub(super) fn cli_status_visible(&self) -> bool {
        self.cli_problem().is_some()
    }

    pub(super) fn show_cli_status(&mut self, ui: &mut Ui) {
        if let Some(problem) = self.cli_problem() {
            ui.add(egui::Label::new(RichText::new(problem).small().color(theme::palette(ui).error)).truncate());
        }
    }

    /// Who approves what a coding CLI does: Taceta each time, or the CLI's
    /// own settings. Always in view, and marked when nothing is asked here.
    /// What GPT, Grok and Claude use, and where their one Taceta control is.
    pub(super) fn show_cli_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        let palette = theme::palette(ui);
        theme::card(ui.visuals().faint_bg_color, palette.border, 14, 16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong(text(language, "各社の公式CLI", "Official vendor CLIs"));
            ui.add_space(4.0);
            ui.label(RichText::new(text(language,
                "GPTはCodex、GrokはGrok CLI、ClaudeはClaude Codeを、このMacに入っているまま使います。ログイン、システムプロンプト、各CLIの設定は各社のものがそのまま効きます。サインインは各CLIで行ってください。",
                "GPT uses Codex, Grok the Grok CLI and Claude Claude Code, as installed on this Mac. Each vendor's login, system prompt and CLI settings apply as they are. Sign in with each CLI itself.")).weak());
            ui.add_space(6.0);
            ui.label(RichText::new(text(language,
                "会話と作業に必要な内容は、GPTはOpenAI、GrokはxAI、ClaudeはAnthropicへ送られます。",
                "The content a conversation or task needs is sent to OpenAI for GPT, xAI for Grok and Anthropic for Claude.")).weak());
            ui.add_space(6.0);
            ui.label(RichText::new(text(language,
                "Codeモードで操作を毎回確認するか自動で承認するかは、入力欄の横のボタンで切り替えます。",
                "Whether Code mode asks before each action or approves automatically is switched with the button beside the message box.")).weak());
        });
    }

    pub(super) fn show_cli_approval_chip(&mut self, ui: &mut Ui) {
        if self.state.active_conversation().cli_chat {
            return;
        }
        let language = self.language();
        let control = app_shell_control_metrics(ui.ctx());
        let ask = self.state.cli_ask_every_time;
        let label = if ask {
            RichText::new(text(language, "毎回確認", "Ask every time"))
        } else {
            RichText::new(text(language, "自動承認", "Automatic")).color(theme::palette(ui).warning)
        };
        let mut selected = ask;
        ui.add_enabled_ui(!self.is_generating(), |ui| {
            let (response, _) = egui::containers::menu::MenuButton::from_button(
                Button::new(label)
                    .min_size(Vec2::new(0.0, control.row_height))
                    .right_text("▼")
                    .corner_radius(control.row_height / 2.0),
            )
            .ui(ui, |ui| {
                ui.spacing_mut().button_padding.y = control.vertical_padding;
                show_app_shell_menu_row(ui, &mut selected, true, text(language, "毎回確認", "Ask every time"));
                show_app_shell_menu_row(ui, &mut selected, false, text(language, "自動承認", "Automatic"));
            });
            response.on_hover_text(if ask && self.state.inference_provider == InferenceProvider::Gpt {
                text(
                    language,
                    "操作の承認：ファイルの変更とコマンドの実行をこの画面で確認します。Codexが安全と判断する読み取りだけのコマンドは確認なしで実行されます。書き込みはこのフォルダーの中だけ、コマンドの通信は無効です。",
                    "Approval: file changes and commands are confirmed here. Read-only commands Codex considers safe run without asking. Writes stay inside this folder, and commands have no network access.",
                )
            } else if ask {
                text(
                    language,
                    "操作の承認：ファイルの変更とコマンドの実行は、1件ずつこの画面で確認します。",
                    "Approval: each file change and command is confirmed here before it runs.",
                )
            } else {
                text(
                    language,
                    "操作の承認：CLI自身の権限設定に従います。設定によっては確認なしでファイルの変更とコマンドの実行が行われます。",
                    "Approval: the CLI's own permission settings decide. Depending on them, files change and commands run without confirmation.",
                )
            });
        });
        self.state.cli_ask_every_time = selected;
    }

    pub(super) fn show_cli_menu_items(&mut self, ui: &mut Ui) {
        let language = self.language();
        let version = match self.cli_ui.readiness.get(&self.state.inference_provider) {
            Some(Ok(readiness)) => readiness.version.clone().unwrap_or_default(),
            _ => String::new(),
        };
        ui.label(RichText::new(format!("{version} · {}", if self.state.active_conversation().cli_chat {
            text(
                language,
                "会話だけを行います。ファイルの読み書きもコマンドの実行もしません。フォルダーは使いません。",
                "Conversation only. No file is read or changed and no command runs. No folder is used.",
            )
        } else {
            text(
                language,
                "普段のCLIのログインと設定を使います。会話の文脈はCLIが管理します。",
                "Uses your normal CLI login and settings. The CLI manages its own context.",
            )
        })).small().weak());
        if let Some(usage) = &self.state.active_conversation().cli_usage {
            ui.label(
                RichText::new(format!(
                    "{}: {} / {} · {}: {}",
                    text(language, "入力 / 出力トークン", "Input / output tokens"),
                    usage.input_tokens,
                    usage.output_tokens,
                    text(language, "コンテキスト上限", "Context capacity"),
                    usage
                        .context_window
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| text(language, "未取得", "Unavailable").into())
                ))
                .small()
                .weak(),
            );
        }
    }

    pub(super) fn start_cli_send(&mut self) {
        let language = self.language();
        let provider = self.state.inference_provider;
        let Some(agent) = self.cli_ui.agent(provider) else {
            return;
        };
        if self.is_generating() {
            return;
        }
        let accepts_attachments = agent.accepts_attachments();
        if self.state.draft.trim().is_empty() && (self.state.pending_attachments.is_empty() || !accepts_attachments) {
            return;
        }
        let chat = self.state.active_conversation().cli_chat;
        let workspace = if chat {
            let folder = self.cli_ui.chat_workspace.clone();
            if let Err(error) = std::fs::create_dir_all(&folder) {
                self.notice = Some(Notice {
                    kind: NoticeKind::Error,
                    text: format!("{}: {error}", folder.display()),
                });
                return;
            }
            folder
        } else if let Some(workspace) = self.state.active_conversation().workspace.clone() {
            workspace
        } else {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(language, "フォルダーを選ぶか、「Chat」に切り替えてください。", "Choose a folder, or switch to Chat.").to_owned(),
            });
            return;
        };
        let model = self.selected_model().cloned();
        if accepts_attachments
            && !model.as_ref().is_some_and(|model| model.vision)
            && self.state.pending_attachments.iter().any(|attachment| matches!(attachment.payload, AttachmentPayload::Image { .. }))
        {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "このモデルは画像入力に対応していません。画像は送信されませんでした。",
                    "This model does not support image input. Nothing was sent.",
                )
                .to_owned(),
            });
            return;
        }
        let effort = match self.selected_thinking_mode() {
            ThinkingMode::Effort(effort) => Some(effort.as_str().to_owned()),
            _ => None,
        };
        let session = self.state.active_conversation().cli_sessions.get(&provider).cloned();
        // Turns this session has not seen go with the request. The draft
        // stays in place while they first have to be summarized.
        let Some(earlier) = self.handover_or_compact(
            session.as_ref().map_or(0, |session| session.seen_messages),
            super::compaction::handover_budget(model.as_ref().and_then(|model| model.context_length)),
            {
                let (agent, workspace) = (agent.clone(), workspace.clone());
                let model = model.as_ref().map(|model| model.name.clone());
                move || super::compaction::Summarizer::Cli { agent, model, workspace }
            },
        ) else {
            return;
        };
        let prompt = std::mem::take(&mut self.state.draft);
        let attachments = if accepts_attachments { std::mem::take(&mut self.state.pending_attachments) } else { Vec::new() };
        let title_fallback = attachments.first().map_or_else(|| "Taceta".to_owned(), |attachment| attachment.name.clone());
        let session_prompt = match &earlier {
            Some(earlier) => with_prior_context(earlier, &prompt),
            None => prompt.clone(),
        };
        let conversation_id = self.state.active_conversation_id;
        let conversation = self.state.active_conversation_mut();
        if conversation.should_generate_title() {
            conversation.title = conversation_title(&prompt, &title_fallback);
        }
        let mut user_message = ChatMessage::new_user(prompt);
        user_message.attachments = attachments.clone();
        conversation.messages.push(user_message);
        let assistant = ChatMessage::new_assistant("");
        let assistant_id = assistant.id;
        conversation.messages.push(assistant);
        self.notice = None;
        let request = RunRequest {
            session_id: session.map(|session| session.session_id),
            workspace,
            model: model.map(|model| model.name),
            effort,
            prompt: session_prompt,
            attachments,
            approval: if chat {
                ApprovalPolicy::NoTools
            } else if self.state.cli_ask_every_time {
                ApprovalPolicy::AskEveryTime
            } else {
                ApprovalPolicy::FollowCli
            },
            max_duration_secs: self.state.agent_max_duration_secs,
        };
        let (event_tx, events) = mpsc::unbounded_channel();
        let (controls, control_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let session_id = request.session_id.clone();
        let task = self.runtime.spawn(async move {
            let outcome = agent.run(request, event_tx, control_rx, cancel_rx).await;
            let _ = result_tx.send(outcome);
        });
        self.cli_ui.active = Some(ActiveCli {
            task,
            conversation_id,
            assistant_id,
            provider,
            events,
            controls,
            cancel,
            result,
            tools: Vec::new(),
            approvals: VecDeque::new(),
            questions: VecDeque::new(),
            session_id,
            no_tools: chat,
        });
        self.scroll_to_bottom = true;
        self.jump_to_latest = true;
    }

    pub(super) fn drain_cli_work(&mut self) {
        if let Some((provider, receiver)) = &self.cli_ui.readiness_result {
            if let Ok(readiness) = receiver.try_recv() {
                let provider = *provider;
                self.cli_ui.readiness.insert(provider, readiness);
                self.cli_ui.readiness_result = None;
            }
        }
        let mut events = Vec::new();
        let mut outcome = None;
        if let Some(active) = self.cli_ui.active.as_mut() {
            while let Ok(event) = active.events.try_recv() {
                events.push(event);
            }
            match active.result.try_recv() {
                Ok(result) => outcome = Some(result),
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    outcome = Some(RunOutcome::failed(
                        active.session_id.clone(),
                        "The CLI run ended without reporting a result.",
                    ));
                }
                Err(std_mpsc::TryRecvError::Empty) => {}
            }
        }
        for event in events {
            self.apply_cli_event(event);
        }
        if let Some(outcome) = outcome {
            // Output sent just before the result still belongs to this turn.
            let mut late = Vec::new();
            if let Some(active) = self.cli_ui.active.as_mut() {
                while let Ok(event) = active.events.try_recv() {
                    late.push(event);
                }
            }
            for event in late {
                self.apply_cli_event(event);
            }
            self.finish_cli_run(outcome);
        }
    }

    fn apply_cli_event(&mut self, event: Event) {
        let Some(active) = self.cli_ui.active.as_mut() else {
            return;
        };
        let (conversation_id, assistant_id, provider) =
            (active.conversation_id, active.assistant_id, active.provider);
        match event {
            Event::SessionReady { session_id } => {
                active.session_id = Some(session_id.clone());
                if let Some(conversation) =
                    self.state.conversations.iter_mut().find(|conversation| conversation.id == conversation_id)
                {
                    // Recorded at once so a crash mid-turn still finds the session.
                    let seen = conversation.cli_sessions.get(&provider).map_or(0, |session| session.seen_messages);
                    conversation
                        .cli_sessions
                        .insert(provider, crate::persistence::CliSession { session_id, seen_messages: seen });
                }
            }
            Event::MessageDelta(delta) => {
                self.update_assistant(conversation_id, assistant_id, |message| message.content.push_str(&delta));
            }
            Event::ThinkingDelta(delta) => {
                self.update_assistant(conversation_id, assistant_id, |message| message.thinking.push_str(&delta));
            }
            Event::Tool(item) => match active.tools.iter_mut().find(|tool| tool.id == item.id) {
                Some(existing) => *existing = item,
                None => active.tools.push(item),
            },
            Event::Approval(request) if active.no_tools => {
                let _ = active.controls.send(Control::Approval { id: request.id, approved: false });
            }
            Event::Approval(request) => {
                active.approvals.push_back((request, std::time::Instant::now()));
            }
            Event::Questions { id, questions } => {
                let answers = vec![String::new(); questions.len()];
                active.questions.push_back(PendingQuestions { id, questions, answers });
            }
            Event::ApprovalResolved { id } => {
                active.approvals.retain(|(request, _)| request.id != id);
                active.questions.retain(|request| request.id != id);
            }
            Event::Usage(usage) => {
                if let Some(conversation) =
                    self.state.conversations.iter_mut().find(|conversation| conversation.id == conversation_id)
                {
                    conversation.cli_usage = Some(crate::persistence::TokenUsage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        context_window: usage.context_window,
                    });
                }
            }
        }
    }

    fn finish_cli_run(&mut self, outcome: RunOutcome) {
        let Some(active) = self.cli_ui.active.take() else {
            return;
        };
        let language = self.language();
        let actions: Vec<ActionRecord> = active
            .tools
            .iter()
            .filter(|tool| tool.status != ToolStatus::Running)
            .map(|tool| ActionRecord {
                title: tool.title.clone(),
                detail: tool.detail.clone(),
                diff: tool.diff.clone(),
                succeeded: tool.status == ToolStatus::Completed,
            })
            .collect();
        let completed = outcome.status == RunStatus::Completed;
        let session_id = outcome.session_id.or(active.session_id);
        if let Some(conversation) =
            self.state.conversations.iter_mut().find(|conversation| conversation.id == active.conversation_id)
        {
            let answered = conversation
                .messages
                .iter_mut()
                .find(|message| message.id == active.assistant_id)
                .map(|message| {
                    message.actions = actions;
                    // A cut-off answer stays visible but is not handed to another engine.
                    message.interrupted = !completed;
                    !message.content.trim().is_empty() || !message.actions.is_empty()
                })
                .unwrap_or(false);
            if !answered {
                conversation.messages.retain(|message| message.id != active.assistant_id);
            }
            if let Some(session_id) = session_id {
                // The CLI's session holds this turn whether or not it finished.
                let seen_messages = conversation.messages.len();
                conversation
                    .cli_sessions
                    .insert(active.provider, crate::persistence::CliSession { session_id, seen_messages });
            }
        }
        self.notice = match outcome.status {
            RunStatus::Completed => self.notice.take(),
            RunStatus::Interrupted => Some(Notice {
                kind: NoticeKind::Info,
                text: text(language, "作業を停止しました。", "The task was stopped.").to_owned(),
            }),
            RunStatus::LimitReached => Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "時間の上限に達したため作業を停止しました。続きは次の送信で再開できます。",
                    "The task stopped at its time limit. Send again to continue.",
                )
                .to_owned(),
            }),
            RunStatus::Failed => Some(Notice {
                kind: NoticeKind::Error,
                text: outcome.error.unwrap_or_else(|| {
                    text(language, "作業は失敗しました。", "The task failed.").to_owned()
                }),
            }),
        };
        self.scroll_to_bottom = true;
    }

    pub(super) fn stop_cli_run(&mut self) {
        if let Some(active) = &self.cli_ui.active {
            let _ = active.cancel.send(true);
        }
    }

    pub(super) fn abort_cli_on_exit(&mut self) {
        if let Some(active) = self.cli_ui.active.take() {
            let _ = active.cancel.send(true);
            // Dropping the run ends the CLI and the processes it started.
            active.task.abort();
        }
    }

    pub(super) fn show_cli_chat(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        self.show_transcript(root_ui, "cli-transcript", |app, ui| {
            let conversation = app.state.active_conversation();
            if conversation.messages.is_empty() && !conversation.cli_chat {
                ui.label(text(
                    language,
                    "フォルダーとモデルを選び、実行したいことを入力してください。",
                    "Choose a folder and model, then describe the work.",
                ));
            }
            for message in &conversation.messages {
                app.show_message(ui, message, false);
            }
            if let Some(active) = &app.cli_ui.active {
                if active.conversation_id == conversation.id {
                    for tool in &active.tools {
                        show_action(
                            ui,
                            (active.assistant_id, &tool.id),
                            match tool.status {
                                ToolStatus::Running => super::ActionMark::Running,
                                ToolStatus::Completed => super::ActionMark::Done,
                                ToolStatus::Failed => super::ActionMark::Failed,
                            },
                            &tool.title,
                            &tool.detail,
                            tool.diff.as_deref(),
                        );
                    }
                    // Says what is happening now, so a long turn is not a blank wait.
                    let answer = conversation.messages.iter().find(|message| message.id == active.assistant_id);
                    let running = active.tools.iter().rev().find(|tool| tool.status == ToolStatus::Running);
                    let status = match (running, answer) {
                        (Some(tool), _) => format!("{}: {}", text(language, "実行中", "Running"), tool.title),
                        (None, Some(answer)) if !answer.content.is_empty() => {
                            text(language, "回答を書いています…", "Writing the answer…").to_owned()
                        }
                        (None, _) if !active.tools.is_empty() => {
                            text(language, "次の手順を考えています…", "Deciding the next step…").to_owned()
                        }
                        (None, Some(answer)) if !answer.thinking.is_empty() => text(language, "考えています…", "Thinking…").to_owned(),
                        _ => text(language, "応答を待っています…", "Waiting for a response…").to_owned(),
                    };
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(status).weak());
                    });
                }
            }
        });
    }

    pub(super) fn show_cli_approval(&mut self, ctx: &Context) {
        let language = self.language();
        let Some(active) = self.cli_ui.active.as_ref() else {
            return;
        };
        let Some((request, shown_at)) = active.approvals.front() else {
            self.show_cli_questions(ctx);
            return;
        };
        let settled = shown_at.elapsed() >= APPROVAL_SETTLE;
        if !settled {
            ctx.request_repaint_after(APPROVAL_SETTLE);
        }
        let waiting = active.approvals.len() - 1;
        let mut decision = None;
        egui::Window::new(text(language, "実行内容の確認", "Approve this action"))
            .id(egui::Id::new("taceta-cli-approval"))
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.strong(&request.title);
                if waiting > 0 {
                    ui.label(
                        RichText::new(match language {
                            AppShellLanguage::Japanese => format!("このあと {waiting} 件の確認があります。"),
                            AppShellLanguage::English => format!("{waiting} more to confirm after this."),
                        })
                        .small()
                        .weak(),
                    );
                }
                ScrollArea::both().max_height(440.0).show(ui, |ui| show_diff_text(ui, &request.detail));
                ui.add_enabled_ui(settled, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button(text(language, "この1回を許可", "Approve once")).clicked() {
                            decision = Some(true);
                        }
                        if ui.button(text(language, "拒否", "Deny")).clicked() {
                            decision = Some(false);
                        }
                    });
                });
            });
        if let Some(approved) = decision {
            self.decide_cli_approval(approved);
        }
    }

    /// The questions the CLI is waiting on, answered together.
    fn show_cli_questions(&mut self, ctx: &Context) {
        let language = self.language();
        let Some(active) = self.cli_ui.active.as_mut() else {
            return;
        };
        let Some(request) = active.questions.front_mut() else {
            return;
        };
        let mut send = false;
        let mut stop = false;
        egui::Window::new(text(language, "質問への回答", "Answer the questions"))
            .id(egui::Id::new("taceta-cli-questions"))
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                    for (index, question) in request.questions.iter().enumerate() {
                        ui.push_id(&question.id, |ui| {
                            ui.strong(&question.header);
                            ui.label(&question.question);
                            for option in &question.options {
                                if ui
                                    .selectable_label(request.answers[index] == option.label, &option.label)
                                    .on_hover_text(&option.description)
                                    .clicked()
                                {
                                    request.answers[index] = option.label.clone();
                                }
                            }
                            ui.add(
                                TextEdit::singleline(&mut request.answers[index])
                                    .password(question.is_secret)
                                    .hint_text(text(language, "回答を入力", "Type an answer")),
                            );
                            ui.add_space(12.0);
                        });
                    }
                });
                ui.horizontal(|ui| {
                    let answered = request.answers.iter().all(|answer| !answer.trim().is_empty());
                    if ui.add_enabled(answered, Button::new(text(language, "回答を送る", "Send answers"))).clicked() {
                        send = true;
                    }
                    if ui.button(text(language, "実行を停止", "Stop run")).clicked() {
                        stop = true;
                    }
                });
            });
        if send {
            if let Some(request) = active.questions.pop_front() {
                let answers = request.questions.into_iter().zip(request.answers).map(|(question, answer)| (question.id, answer)).collect();
                let _ = active.controls.send(Control::Answers { id: request.id, answers });
            }
        }
        if stop {
            self.stop_cli_run();
        }
    }

    fn decide_cli_approval(&mut self, approved: bool) {
        if let Some(active) = self.cli_ui.active.as_mut() {
            if let Some((request, _)) = active.approvals.pop_front() {
                let _ = active.controls.send(Control::Approval { id: request.id, approved });
            }
        }
    }
}

pub(super) fn show_action(
    ui: &mut Ui,
    id: impl std::hash::Hash,
    mark: super::ActionMark,
    title: &str,
    detail: &str,
    diff: Option<&str>,
) {
    super::show_action_row(ui, ("cli-action", id), mark, title, false, |ui| {
        ScrollArea::both().max_height(360.0).show(ui, |ui| {
            if let Some(diff) = diff {
                show_diff_text(ui, diff);
            }
            if !detail.is_empty() {
                ui.monospace(detail);
            }
        });
    });
}

/// Draws text in which lines starting with `+ ` and `- ` are a change.
fn show_diff_text(ui: &mut Ui, text: &str) {
    let palette = theme::palette(ui);
    for line in text.lines() {
        let rich = RichText::new(line).monospace();
        ui.label(if line.starts_with("+ ") {
            rich.color(palette.success)
        } else if line.starts_with("- ") {
            rich.color(palette.error)
        } else {
            rich
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_manager_tests::{RecordingServices, test_app};
    use taceta::prior_context::{Plan, plan};

    fn active_fixture(app: &mut TacetaApp) -> (mpsc::UnboundedSender<Event>, mpsc::UnboundedReceiver<Control>, std_mpsc::Sender<RunOutcome>) {
        let (event_tx, events) = mpsc::unbounded_channel();
        let (controls, control_rx) = mpsc::unbounded_channel();
        let (cancel, _cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let conversation = app.state.active_conversation_mut();
        conversation.messages.push(ChatMessage::new_user("do it"));
        let assistant = ChatMessage::new_assistant("");
        let assistant_id = assistant.id;
        conversation.messages.push(assistant);
        app.cli_ui.active = Some(ActiveCli {
            task: app.runtime.spawn(std::future::pending()),
            conversation_id: app.state.active_conversation_id,
            assistant_id,
            provider: InferenceProvider::Claude,
            events,
            controls,
            cancel,
            result,
            tools: Vec::new(),
            approvals: VecDeque::new(),
            questions: VecDeque::new(),
            session_id: None,
            no_tools: false,
        });
        (event_tx, control_rx, result_tx)
    }

    fn tool(id: &str, status: ToolStatus) -> ToolItem {
        ToolItem { id: id.into(), title: format!("Edit {id}"), detail: String::new(), diff: Some("+ x".into()), status }
    }

    #[test]
    fn a_finished_turn_is_recorded_in_the_conversation_with_its_actions_and_session() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (events, _controls, result) = active_fixture(&mut app);
        events.send(Event::SessionReady { session_id: "session-1".into() }).unwrap();
        events.send(Event::ThinkingDelta("reasoning".into())).unwrap();
        events.send(Event::Tool(tool("a", ToolStatus::Running))).unwrap();
        events.send(Event::Tool(tool("a", ToolStatus::Completed))).unwrap();
        events.send(Event::Tool(tool("b", ToolStatus::Running))).unwrap();
        events.send(Event::MessageDelta("All ".into())).unwrap();
        result
            .send(RunOutcome { session_id: Some("session-1".into()), status: RunStatus::Completed, error: None })
            .unwrap();
        // Sent after the result was queued, as a fast CLI can do.
        events.send(Event::MessageDelta("done".into())).unwrap();
        app.drain_cli_work();

        assert!(app.cli_ui.active.is_none());
        let conversation = app.state.active_conversation();
        let answer = conversation.messages.last().unwrap();
        assert_eq!(answer.content, "All done");
        assert_eq!(answer.thinking, "reasoning");
        assert!(!answer.interrupted);
        // Only finished actions are part of the record.
        assert_eq!(answer.actions.len(), 1);
        assert_eq!(answer.actions[0].title, "Edit a");
        assert!(answer.actions[0].succeeded);
        assert_eq!(
            conversation.cli_sessions[&InferenceProvider::Claude],
            crate::persistence::CliSession { session_id: "session-1".into(), seen_messages: 2 }
        );
    }

    #[test]
    fn a_stopped_turn_keeps_its_partial_answer_out_of_what_other_engines_receive() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (events, _controls, result) = active_fixture(&mut app);
        events.send(Event::SessionReady { session_id: "session-1".into() }).unwrap();
        events.send(Event::MessageDelta("half".into())).unwrap();
        result
            .send(RunOutcome { session_id: Some("session-1".into()), status: RunStatus::Interrupted, error: None })
            .unwrap();
        app.drain_cli_work();
        let conversation = app.state.active_conversation();
        assert!(conversation.messages.last().unwrap().interrupted);
        assert_eq!(conversation.cli_sessions[&InferenceProvider::Claude].seen_messages, 2);
        let Plan::Ready(Some(handed_over)) = plan(None, &conversation.messages, 0, 10_000) else {
            panic!("the conversation fits and must be handed over as it is");
        };
        assert!(handed_over.contains("do it") && !handed_over.contains("half"));
    }

    #[test]
    fn a_turn_that_produced_nothing_leaves_no_empty_answer() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_events, _controls, result) = active_fixture(&mut app);
        result.send(RunOutcome::failed(None, "The CLI was not found.")).unwrap();
        app.drain_cli_work();
        let conversation = app.state.active_conversation();
        assert_eq!(conversation.messages.len(), 1);
        assert!(conversation.cli_sessions.is_empty());
        assert_eq!(app.notice.as_ref().unwrap().text, "The CLI was not found.");
    }

    #[test]
    fn approvals_are_answered_in_order_and_withdrawn_ones_disappear() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (events, mut controls, _result) = active_fixture(&mut app);
        let request = |title: &str| ApprovalRequest { id: Uuid::new_v4(), title: title.into(), detail: String::new() };
        let (first, second, third) = (request("one"), request("two"), request("three"));
        for request in [&first, &second, &third] {
            events.send(Event::Approval(request.clone())).unwrap();
        }
        events.send(Event::ApprovalResolved { id: second.id }).unwrap();
        app.drain_cli_work();
        app.decide_cli_approval(true);
        app.decide_cli_approval(false);
        app.decide_cli_approval(true);
        assert_eq!(controls.try_recv().unwrap(), Control::Approval { id: first.id, approved: true });
        assert_eq!(controls.try_recv().unwrap(), Control::Approval { id: third.id, approved: false });
        assert!(controls.try_recv().is_err());
    }

    #[test]
    fn model_options_become_selectable_models_with_their_efforts() {
        let descriptor = model_descriptor(ModelOption {
            id: "opus".into(),
            label: "Opus".into(),
            context_window: Some(256_000),
            efforts: vec!["low".into(), "high".into(), "not-an-effort".into()],
            default_effort: Some("high".into()),
            images: false,
        });
        assert_eq!(descriptor.name, "opus");
        assert_eq!(descriptor.context_length, Some(256_000));
        assert_eq!(
            descriptor.thinking,
            ThinkingCapability::Efforts {
                supported: vec![ReasoningEffort::Low, ReasoningEffort::High],
                default: Some(ReasoningEffort::High),
            }
        );
        let unnamed_default = model_descriptor(ModelOption {
            id: "sonnet".into(),
            label: "Sonnet".into(),
            context_window: None,
            efforts: ["low", "medium", "high", "xhigh", "max"].map(str::to_owned).to_vec(),
            default_effort: None,
            images: false,
        });
        assert!(matches!(
            unnamed_default.thinking,
            ThinkingCapability::Efforts { default: Some(ReasoningEffort::High), .. }
        ));
    }
}
